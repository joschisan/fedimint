mod api;
mod complete_sm;
mod db;
pub mod events;
mod receive_sm;
mod send_sm;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

use crate::executor::ModuleExecutor;
use crate::module::ClientContext;
use crate::transaction::{Output, TransactionBuilder};
use picomint_core::wire;
use anyhow::{anyhow, ensure};
use async_trait::async_trait;
use bitcoin::hashes::sha256;
use bitcoin::secp256k1::Message;
use events::{
    CompleteEvent, ReceiveEvent, ReceiveFailureEvent, ReceiveRefundEvent, ReceiveSuccessEvent,
    SendCancelEvent, SendEvent, SendSuccessEvent,
};
use lightning_invoice::Bolt11Invoice;
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::hex::ToHex;
use picomint_core::ln::config::LightningConfigConsensus;
use picomint_core::ln::contracts::{IncomingContract, PaymentImage};
use picomint_core::ln::gateway_api::SendPaymentPayload;
use picomint_core::ln::{LightningInvoice, LightningOutput};
use picomint_core::secp256k1::Keypair;
use picomint_core::task::TaskGroup;
use picomint_core::{Amount, OutPoint, PeerId, secp256k1};
use picomint_encoding::{Decodable, Encodable};

use crate::secret::Secret;
use receive_sm::ReceiveStateMachine;
use secp256k1::schnorr::Signature;
use send_sm::SendStateMachine;
use serde::{Deserialize, Serialize};
use tpe::{AggregatePublicKey, PublicKeyShare};
use tracing::warn;

use self::complete_sm::{CompleteSMCommon, CompleteSMState, CompleteStateMachine};

/// Lightning CLTV Delta in blocks
pub const EXPIRATION_DELTA_MINIMUM: u64 = 144;

#[derive(Encodable)]
enum RootSecretPath {
    Node,
}

impl GatewayClientModule {
    pub async fn new(
        federation_id: FederationId,
        cfg: LightningConfigConsensus,
        context: ClientContext,
        mint: Arc<crate::mint::MintClientModule>,
        gateway: Arc<dyn IGatewayClient>,
        module_root_secret: &Secret,
        task_group: &TaskGroup,
    ) -> anyhow::Result<GatewayClientModule> {
        let keypair = module_root_secret.child(&RootSecretPath::Node).to_secp_keypair();

        let sm_context = GwSmContext {
            client_ctx: context.clone(),
            mint: mint.clone(),
            input_fee: cfg.input_fee,
            keypair,
            tpe_agg_pk: cfg.tpe_agg_pk,
            tpe_pks: cfg.tpe_pks.clone(),
            gateway: gateway.clone(),
        };

        let send_executor = ModuleExecutor::new(
            context.db().clone(),
            sm_context.clone(),
            task_group.clone(),
        )
        .await;
        let receive_executor = ModuleExecutor::new(
            context.db().clone(),
            sm_context.clone(),
            task_group.clone(),
        )
        .await;
        let complete_executor =
            ModuleExecutor::new(context.db().clone(), sm_context, task_group.clone())
                .await;

        Ok(GatewayClientModule {
            federation_id,
            cfg,
            client_ctx: context,
            mint,
            keypair,
            gateway,
            send_executor,
            receive_executor,
            complete_executor,
        })
    }
}

#[derive(Debug, Clone)]
pub struct GatewayClientModule {
    pub federation_id: FederationId,
    pub cfg: LightningConfigConsensus,
    pub client_ctx: ClientContext,
    pub mint: Arc<crate::mint::MintClientModule>,
    pub keypair: Keypair,
    pub gateway: Arc<dyn IGatewayClient>,
    send_executor: ModuleExecutor<SendStateMachine>,
    receive_executor: ModuleExecutor<ReceiveStateMachine>,
    complete_executor: ModuleExecutor<CompleteStateMachine>,
}

/// Lean context handed to per-SM executors. Holds `Arc<MintClientModule>`
/// (no cycle — gw owns its executor → ctx → mint, mint does not reach back
/// into gw) but does NOT hold `Arc<GatewayClientModule>` itself.
#[derive(Debug, Clone)]
pub struct GwSmContext {
    pub client_ctx: ClientContext,
    pub mint: Arc<crate::mint::MintClientModule>,
    pub input_fee: Amount,
    pub keypair: Keypair,
    pub tpe_agg_pk: AggregatePublicKey,
    pub tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
    pub gateway: Arc<dyn IGatewayClient>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Decodable, Encodable)]
pub enum FinalReceiveState {
    Success([u8; 32]),
    Refunded,
    Failure,
}

impl GatewayClientModule {
    pub fn input_fee(&self) -> Amount {
        self.cfg.input_fee
    }

    pub fn output_fee(&self) -> Amount {
        self.cfg.output_fee
    }

    pub async fn send_payment(
        &self,
        payload: SendPaymentPayload,
    ) -> anyhow::Result<Result<[u8; 32], Signature>> {
        // The operation id is equal to the contract id which also doubles as the
        // message signed by the gateway via the forfeit signature to forfeit
        // the gateways claim to a contract in case of cancellation. We only create a
        // forfeit signature after we have started the send state machine to
        // prevent replay attacks with a previously cancelled outgoing contract
        let operation_id = OperationId::from_encodable(&payload.contract.clone());

        // Since the following four checks may only fail due to client side
        // programming error we do not have to enable cancellation and can check
        // them before we start the state machine.
        ensure!(
            payload.contract.claim_pk == self.keypair.public_key(),
            "The outgoing contract is keyed to another gateway"
        );

        // This prevents DOS attacks where an attacker submits a different invoice.
        ensure!(
            secp256k1::SECP256K1
                .verify_schnorr(
                    &payload.auth,
                    &Message::from_digest(
                        *payload.invoice.consensus_hash::<sha256::Hash>().as_ref()
                    ),
                    &payload.contract.refund_pk.x_only_public_key().0,
                )
                .is_ok(),
            "Invalid auth signature for the invoice data"
        );

        // We need to check that the contract has been confirmed by the federation
        // before we start the state machine to prevent DOS attacks.
        let (contract_id, expiration) = self
            .client_ctx
            .module_api()
            .gw_outgoing_contract_expiration(payload.outpoint)
            .await
            .map_err(|_| anyhow!("The gateway can not reach the federation"))?
            .ok_or(anyhow!("The outgoing contract has not yet been confirmed"))?;

        ensure!(
            contract_id == payload.contract.contract_id(),
            "Contract Id returned by the federation does not match contract in request"
        );

        let (payment_hash, amount) = match &payload.invoice {
            LightningInvoice::Bolt11(invoice) => (
                invoice.payment_hash(),
                invoice
                    .amount_milli_satoshis()
                    .ok_or(anyhow!("Invoice is missing amount"))?,
            ),
        };

        ensure!(
            PaymentImage::Hash(*payment_hash) == payload.contract.payment_image,
            "The invoices payment hash does not match the contracts payment hash"
        );

        let min_contract_amount = self
            .gateway
            .min_contract_amount(&payload.federation_id, amount)
            .await?;

        let dbtx = self.client_ctx.db().begin_write();
        let tx = dbtx.as_ref();

        if tx.insert(&db::OPERATION, &operation_id, &()).is_some() {
            return Ok(self.subscribe_send(operation_id).await);
        }

        self.send_executor.add_state_machine_dbtx(
            &tx,
            SendStateMachine {
                operation_id,
                outpoint: payload.outpoint,
                contract: payload.contract.clone(),
                max_delay: expiration.saturating_sub(EXPIRATION_DELTA_MINIMUM),
                min_contract_amount,
                invoice: payload.invoice.clone(),
                claim_keypair: self.keypair,
            },
        );

        self.client_ctx
            .log_event(
                &tx,
                operation_id,
                SendEvent {
                    outpoint: payload.outpoint,
                    invoice: payload.invoice,
                },
            );

        dbtx.commit();

        Ok(self.subscribe_send(operation_id).await)
    }

    pub async fn subscribe_send(&self, operation_id: OperationId) -> Result<[u8; 32], Signature> {
        use futures::StreamExt as _;

        let mut stream = self.client_ctx.subscribe_operation_events(operation_id);
        while let Some(entry) = stream.next().await {
            if let Some(ev) = entry.to_event::<SendSuccessEvent>() {
                return Ok(ev.preimage);
            }
            if let Some(ev) = entry.to_event::<SendCancelEvent>() {
                warn!("Outgoing lightning payment is cancelled");
                return Err(ev.signature);
            }
        }
        unreachable!("subscribe_operation_events only ends at client shutdown")
    }

    pub async fn relay_incoming_htlc(
        &self,
        payment_hash: sha256::Hash,
        incoming_chan_id: u64,
        htlc_id: u64,
        contract: IncomingContract,
        _amount_msat: u64,
    ) -> anyhow::Result<()> {
        let operation_id = OperationId::from_encodable(&contract);

        let refund_keypair = self.keypair;

        let tx_builder = TransactionBuilder::from_output(Output {
            output: wire::Output::Ln(LightningOutput::Incoming(contract.clone())),
            amount: contract.commitment.amount,
            fee: self.cfg.output_fee,
        });

        let dbtx = self.client_ctx.db().begin_write();

        if dbtx
            .as_ref()
            .insert(&db::OPERATION, &operation_id, &())
            .is_some()
        {
            return Ok(());
        }

        let txid = self
            .mint
            .finalize_and_submit_transaction(&dbtx.as_ref(), operation_id, tx_builder)
            .await?;

        let outpoint = OutPoint { txid, out_idx: 0 };

        let receive_sm = ReceiveStateMachine {
            operation_id,
            contract: contract.clone(),
            outpoint,
            refund_keypair,
        };

        self.receive_executor
            .add_state_machine_dbtx(&dbtx.as_ref(), receive_sm);

        let complete_sm = CompleteStateMachine {
            common: CompleteSMCommon {
                operation_id,
                payment_hash,
                incoming_chan_id,
                htlc_id,
            },
            state: CompleteSMState::Pending,
        };

        self.complete_executor
            .add_state_machine_dbtx(&dbtx.as_ref(), complete_sm);

        let event = ReceiveEvent {
            txid: outpoint.txid,
            amount: contract.commitment.amount,
        };

        self.client_ctx
            .log_event(&dbtx.as_ref(), operation_id, event);

        dbtx.commit();

        Ok(())
    }

    pub async fn relay_direct_swap(
        &self,
        contract: IncomingContract,
        _amount_msat: u64,
    ) -> anyhow::Result<FinalReceiveState> {
        let operation_id = OperationId::from_encodable(&contract);

        let refund_keypair = self.keypair;

        let tx_builder = TransactionBuilder::from_output(Output {
            output: wire::Output::Ln(LightningOutput::Incoming(contract.clone())),
            amount: contract.commitment.amount,
            fee: self.cfg.output_fee,
        });

        let dbtx = self.client_ctx.db().begin_write();

        if dbtx
            .as_ref()
            .insert(&db::OPERATION, &operation_id, &())
            .is_some()
        {
            return Ok(self.await_receive(operation_id).await);
        }

        let txid = self
            .mint
            .finalize_and_submit_transaction(&dbtx.as_ref(), operation_id, tx_builder)
            .await?;

        let outpoint = OutPoint { txid, out_idx: 0 };

        let sm = ReceiveStateMachine {
            operation_id,
            contract: contract.clone(),
            outpoint,
            refund_keypair,
        };

        self.receive_executor
            .add_state_machine_dbtx(&dbtx.as_ref(), sm);

        let event = ReceiveEvent {
            txid: outpoint.txid,
            amount: contract.commitment.amount,
        };

        self.client_ctx
            .log_event(&dbtx.as_ref(), operation_id, event);

        dbtx.commit();

        Ok(self.await_receive(operation_id).await)
    }

    pub async fn await_receive(&self, operation_id: OperationId) -> FinalReceiveState {
        await_receive_from_log(&self.client_ctx, operation_id).await
    }

    /// For the given `OperationId`, this function will wait until the Complete
    /// state machine has finished.
    pub async fn await_completion(&self, operation_id: OperationId) {
        use futures::StreamExt as _;

        let mut stream = self
            .client_ctx
            .subscribe_operation_events_typed::<CompleteEvent>(operation_id);
        stream.next().await;
    }
}

pub(crate) async fn await_receive_from_log(
    client_ctx: &ClientContext,
    operation_id: OperationId,
) -> FinalReceiveState {
    use futures::StreamExt as _;

    let mut stream = client_ctx.subscribe_operation_events(operation_id);
    while let Some(entry) = stream.next().await {
        if let Some(ev) = entry.to_event::<ReceiveSuccessEvent>() {
            return FinalReceiveState::Success(ev.preimage);
        }
        if entry.to_event::<ReceiveRefundEvent>().is_some() {
            return FinalReceiveState::Refunded;
        }
        if entry.to_event::<ReceiveFailureEvent>().is_some() {
            return FinalReceiveState::Failure;
        }
    }
    unreachable!("subscribe_operation_events only ends at client shutdown")
}

/// An interface between module implementation and the general `Gateway`
///
/// To abstract away and decouple the core gateway from the modules, the
/// interface between the is expressed as a trait. The core gateway handles
/// lightning operations that require access to the database or lightning node.
#[async_trait]
pub trait IGatewayClient: Debug + Send + Sync {
    /// Use the gateway's lightning node to complete a payment
    async fn complete_htlc(&self, htlc_response: InterceptPaymentResponse);

    /// Try to settle an outgoing payment via a direct swap to another
    /// federation hosted by the same gateway. If the gateway's connected
    /// lightning node is the invoice's payee the gateway dispatches the swap
    /// against the target federation's `GatewayClientModule` and returns
    /// the final receive state along with the target federation id.
    ///
    /// Returns `Ok(None)` when this is not a direct swap.
    async fn try_direct_swap(
        &self,
        invoice: &Bolt11Invoice,
    ) -> anyhow::Result<Option<(FinalReceiveState, FederationId)>>;

    /// Initiates a payment over the Lightning network.
    async fn pay(
        &self,
        invoice: Bolt11Invoice,
        max_delay: u64,
        max_fee: Amount,
    ) -> Result<[u8; 32], LightningRpcError>;

    /// Computes the minimum contract amount necessary for making an outgoing
    /// payment.
    ///
    /// The minimum contract amount must contain transaction fees to cover the
    /// gateway's transaction fee and optionally additional fee to cover the
    /// gateway's Lightning fee if the payment goes over the Lightning
    /// network.
    async fn min_contract_amount(
        &self,
        federation_id: &FederationId,
        amount: u64,
    ) -> anyhow::Result<Amount>;
}

// --- Types shared with picomint-gateway-daemon ---

#[derive(
    thiserror::Error,
    Debug,
    Serialize,
    Deserialize,
    Encodable,
    Decodable,
    Clone,
    Eq,
    PartialEq,
    Hash,
)]
pub enum LightningRpcError {
    #[error("Failed to connect to Lightning node")]
    FailedToConnect,
    #[error("Failed to retrieve node info: {failure_reason}")]
    FailedToGetNodeInfo { failure_reason: String },
    #[error("Failed to retrieve route hints: {failure_reason}")]
    FailedToGetRouteHints { failure_reason: String },
    #[error("Payment failed: {failure_reason}")]
    FailedPayment { failure_reason: String },
    #[error("Failed to route HTLCs: {failure_reason}")]
    FailedToRouteHtlcs { failure_reason: String },
    #[error("Failed to complete HTLC: {failure_reason}")]
    FailedToCompleteHtlc { failure_reason: String },
    #[error("Failed to open channel: {failure_reason}")]
    FailedToOpenChannel { failure_reason: String },
    #[error("Failed to close channel: {failure_reason}")]
    FailedToCloseChannelsWithPeer { failure_reason: String },
    #[error("Failed to get Invoice: {failure_reason}")]
    FailedToGetInvoice { failure_reason: String },
    #[error("Failed to list transactions: {failure_reason}")]
    FailedToListTransactions { failure_reason: String },
    #[error("Failed to get funding address: {failure_reason}")]
    FailedToGetLnOnchainAddress { failure_reason: String },
    #[error("Failed to withdraw funds on-chain: {failure_reason}")]
    FailedToWithdrawOnchain { failure_reason: String },
    #[error("Failed to connect to peer: {failure_reason}")]
    FailedToConnectToPeer { failure_reason: String },
    #[error("Failed to list active channels: {failure_reason}")]
    FailedToListChannels { failure_reason: String },
    #[error("Failed to get balances: {failure_reason}")]
    FailedToGetBalances { failure_reason: String },
    #[error("Failed to sync to chain: {failure_reason}")]
    FailedToSyncToChain { failure_reason: String },
    #[error("Invalid metadata: {failure_reason}")]
    InvalidMetadata { failure_reason: String },
    #[error("Bolt12 Error: {failure_reason}")]
    Bolt12Error { failure_reason: String },
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InterceptPaymentResponse {
    pub incoming_chan_id: u64,
    pub htlc_id: u64,
    pub payment_hash: sha256::Hash,
    pub action: PaymentAction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum PaymentAction {
    Settle(Preimage),
    Cancel,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct Preimage(pub [u8; 32]);

impl std::fmt::Display for Preimage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.encode_hex::<String>())
    }
}
