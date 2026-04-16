mod api;
mod complete_sm;
mod db;
pub mod events;
mod receive_sm;
mod send_sm;

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::sync::Arc;

use anyhow::{anyhow, ensure};
use async_trait::async_trait;
use bitcoin::hashes::sha256;
use bitcoin::secp256k1::Message;
use events::{
    CompleteLightningPaymentEvent, ReceivePaymentStatus, ReceivePaymentUpdateEvent,
    SendPaymentStatus, SendPaymentUpdateEvent,
};
use fedimint_api_client::api::FederationApi;
use fedimint_client_module::executor::ModuleExecutor;
use fedimint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client_module::module::{ClientContext, ClientModule};
use fedimint_client_module::transaction::{ClientOutput, ClientOutputBundle, TransactionBuilder};
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::hex::ToHex;
use fedimint_core::module::{ModuleCommon, ModuleInit};
use fedimint_core::secp256k1::Keypair;
use fedimint_core::{Amount, OutPoint, PeerId, apply, async_trait_maybe_send, secp256k1};
use fedimint_lnv2_common::config::LightningClientConfig;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::SendPaymentPayload;
use fedimint_lnv2_common::{
    LightningCommonInit, LightningInvoice, LightningModuleTypes, LightningOutput, LightningOutputV0,
};
use lightning_invoice::Bolt11Invoice;
use receive_sm::{ReceiveSMState, ReceiveStateMachine};
use secp256k1::schnorr::Signature;
use send_sm::{SendSMState, SendStateMachine};
use serde::{Deserialize, Serialize};
use tpe::{AggregatePublicKey, PublicKeyShare};
use tracing::warn;

use crate::api::GatewayFederationApi;
use crate::complete_sm::{CompleteSMCommon, CompleteSMState, CompleteStateMachine};
use crate::receive_sm::ReceiveSMCommon;
use crate::send_sm::SendSMCommon;

/// LNv2 CLTV Delta in blocks
pub const EXPIRATION_DELTA_MINIMUM_V2: u64 = 144;

#[derive(Debug, Clone)]
pub struct GatewayClientInitV2 {
    pub gateway: Arc<dyn IGatewayClientV2>,
}

impl ModuleInit for GatewayClientInitV2 {
    type Common = LightningCommonInit;
}

#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for GatewayClientInitV2 {
    type Module = GatewayClientModuleV2;

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        let federation_id = *args.federation_id();
        let cfg = args.cfg().clone();
        let client_ctx = args.context();
        let module_api = args.module_api().clone();
        let keypair = args
            .module_root_secret()
            .clone()
            .to_secp_key(fedimint_core::secp256k1::SECP256K1);
        let gateway = self.gateway.clone();
        let task_group = args.task_group().clone();

        let sm_context = GwV2SmContext {
            client_ctx: client_ctx.clone(),
            keypair,
            tpe_agg_pk: cfg.tpe_agg_pk,
            tpe_pks: cfg.tpe_pks.clone(),
            gateway: gateway.clone(),
        };

        let send_executor = ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context.clone(),
            task_group.clone(),
        );
        let receive_executor = ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context.clone(),
            task_group.clone(),
        );
        let complete_executor =
            ModuleExecutor::new(client_ctx.module_db().clone(), sm_context, task_group);

        Ok(GatewayClientModuleV2 {
            federation_id,
            cfg,
            client_ctx,
            module_api,
            keypair,
            gateway,
            send_executor,
            receive_executor,
            complete_executor,
        })
    }
}

#[derive(Debug, Clone)]
pub struct GatewayClientModuleV2 {
    pub federation_id: FederationId,
    pub cfg: LightningClientConfig,
    pub client_ctx: ClientContext<Self>,
    pub module_api: FederationApi,
    pub keypair: Keypair,
    pub gateway: Arc<dyn IGatewayClientV2>,
    send_executor: ModuleExecutor<SendStateMachine>,
    receive_executor: ModuleExecutor<ReceiveStateMachine>,
    complete_executor: ModuleExecutor<CompleteStateMachine>,
}

/// Lean context handed to per-SM executors. Does NOT hold the module itself
/// — that would create a cycle (module → executor → Inner → ctx → module).
#[derive(Debug, Clone)]
pub struct GwV2SmContext {
    pub client_ctx: ClientContext<GatewayClientModuleV2>,
    pub keypair: Keypair,
    pub tpe_agg_pk: AggregatePublicKey,
    pub tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
    pub gateway: Arc<dyn IGatewayClientV2>,
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for GatewayClientModuleV2 {
    type Init = GatewayClientInitV2;
    type Common = LightningModuleTypes;

    async fn start(&self) {
        self.send_executor.start().await;
        self.receive_executor.start().await;
        self.complete_executor.start().await;
    }
    fn input_fee(
        &self,
        _amount: Amount,
        _input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amount> {
        Some(self.cfg.input_fee)
    }

    fn output_fee(
        &self,
        _amount: Amount,
        _output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amount> {
        Some(self.cfg.output_fee)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Decodable, Encodable)]
pub enum FinalReceiveState {
    Rejected,
    Success([u8; 32]),
    Refunded,
    Failure,
}

impl GatewayClientModuleV2 {
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
            .module_api
            .outgoing_contract_expiration(payload.outpoint)
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

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        if tx.insert(&db::OPERATION, &operation_id, &()).is_some() {
            return Ok(self.subscribe_send(operation_id).await);
        }

        self.send_executor
            .add_state_machine_dbtx(
                &tx,
                SendStateMachine {
                    common: SendSMCommon {
                        operation_id,
                        outpoint: payload.outpoint,
                        contract: payload.contract.clone(),
                        max_delay: expiration.saturating_sub(EXPIRATION_DELTA_MINIMUM_V2),
                        min_contract_amount,
                        invoice: payload.invoice,
                        claim_keypair: self.keypair,
                    },
                    state: SendSMState::Sending,
                },
            )
            .await;

        dbtx.commit().await;

        Ok(self.subscribe_send(operation_id).await)
    }

    pub async fn subscribe_send(&self, operation_id: OperationId) -> Result<[u8; 32], Signature> {
        let status =
            await_event::<SendPaymentUpdateEvent, _>(&self.client_ctx, operation_id, |ev| {
                Some(ev.status.clone())
            })
            .await;

        match status {
            SendPaymentStatus::Success(preimage) => Ok(preimage),
            SendPaymentStatus::Cancelled(signature) => {
                warn!("Outgoing lightning payment is cancelled");
                Err(signature)
            }
        }
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

        let client_output = ClientOutput::<LightningOutput> {
            output: LightningOutput::V0(LightningOutputV0::Incoming(contract.clone())),
            amount: contract.commitment.amount,
        };

        let client_output_bundle = self.client_ctx.make_client_outputs(ClientOutputBundle::<
            LightningOutput,
        >::new(vec![
            client_output,
        ]));
        let transaction = TransactionBuilder::new().with_outputs(client_output_bundle);

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        if tx.insert(&db::OPERATION, &operation_id, &()).is_some() {
            return Ok(());
        }

        let range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(&tx, operation_id, transaction)
            .await?;

        let outpoint = OutPoint {
            txid: range.txid(),
            out_idx: 0,
        };

        self.receive_executor
            .add_state_machine_dbtx(
                &tx,
                ReceiveStateMachine {
                    common: ReceiveSMCommon {
                        operation_id,
                        contract,
                        outpoint,
                        refund_keypair,
                    },
                    state: ReceiveSMState::Funding,
                },
            )
            .await;

        self.complete_executor
            .add_state_machine_dbtx(
                &tx,
                CompleteStateMachine {
                    common: CompleteSMCommon {
                        operation_id,
                        payment_hash,
                        incoming_chan_id,
                        htlc_id,
                    },
                    state: CompleteSMState::Pending,
                },
            )
            .await;

        dbtx.commit().await;

        Ok(())
    }

    pub async fn relay_direct_swap(
        &self,
        contract: IncomingContract,
        _amount_msat: u64,
    ) -> anyhow::Result<FinalReceiveState> {
        let operation_id = OperationId::from_encodable(&contract);

        let refund_keypair = self.keypair;

        let client_output = ClientOutput::<LightningOutput> {
            output: LightningOutput::V0(LightningOutputV0::Incoming(contract.clone())),
            amount: contract.commitment.amount,
        };

        let client_output_bundle = self.client_ctx.make_client_outputs(ClientOutputBundle::<
            LightningOutput,
        >::new(vec![
            client_output,
        ]));
        let transaction = TransactionBuilder::new().with_outputs(client_output_bundle);

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        if tx.insert(&db::OPERATION, &operation_id, &()).is_some() {
            return Ok(self.await_receive(operation_id).await);
        }

        let range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(&tx, operation_id, transaction)
            .await?;

        let outpoint = OutPoint {
            txid: range.txid(),
            out_idx: 0,
        };

        self.receive_executor
            .add_state_machine_dbtx(
                &tx,
                ReceiveStateMachine {
                    common: ReceiveSMCommon {
                        operation_id,
                        contract,
                        outpoint,
                        refund_keypair,
                    },
                    state: ReceiveSMState::Funding,
                },
            )
            .await;

        dbtx.commit().await;

        Ok(self.await_receive(operation_id).await)
    }

    pub async fn await_receive(&self, operation_id: OperationId) -> FinalReceiveState {
        let status =
            await_event::<ReceivePaymentUpdateEvent, _>(&self.client_ctx, operation_id, |ev| {
                Some(ev.status.clone())
            })
            .await;

        match status {
            ReceivePaymentStatus::Success(preimage) => FinalReceiveState::Success(preimage),
            ReceivePaymentStatus::Rejected => FinalReceiveState::Rejected,
            ReceivePaymentStatus::Refunded => FinalReceiveState::Refunded,
            ReceivePaymentStatus::Failure => FinalReceiveState::Failure,
        }
    }

    /// For the given `OperationId`, this function will wait until the Complete
    /// state machine has finished.
    pub async fn await_completion(&self, operation_id: OperationId) {
        await_event::<CompleteLightningPaymentEvent, _>(&self.client_ctx, operation_id, |_| {
            Some(())
        })
        .await;
    }
}

pub(crate) async fn await_receive_from_log(
    client_ctx: &ClientContext<GatewayClientModuleV2>,
    operation_id: OperationId,
) -> FinalReceiveState {
    let status = await_event::<ReceivePaymentUpdateEvent, _>(client_ctx, operation_id, |ev| {
        Some(ev.status.clone())
    })
    .await;

    match status {
        ReceivePaymentStatus::Success(preimage) => FinalReceiveState::Success(preimage),
        ReceivePaymentStatus::Rejected => FinalReceiveState::Rejected,
        ReceivePaymentStatus::Refunded => FinalReceiveState::Refunded,
        ReceivePaymentStatus::Failure => FinalReceiveState::Failure,
    }
}

/// Tail the op-scoped sublog, yielding the first match of `predicate` applied
/// to an event of type `E`.
async fn await_event<E, T>(
    client_ctx: &ClientContext<GatewayClientModuleV2>,
    operation_id: OperationId,
    predicate: impl Fn(&E) -> Option<T>,
) -> T
where
    E: fedimint_eventlog::Event + Send + 'static,
{
    use futures::StreamExt as _;

    let mut stream = client_ctx.subscribe_operation_events_typed::<E>(operation_id);
    while let Some(ev) = stream.next().await {
        if let Some(out) = predicate(&ev) {
            return out;
        }
    }
    unreachable!("subscribe_operation_events_typed only ends at client shutdown")
}

/// An interface between module implementation and the general `Gateway`
///
/// To abstract away and decouple the core gateway from the modules, the
/// interface between the is expressed as a trait. The core gateway handles
/// LNv2 operations that require access to the database or lightning node.
#[async_trait]
pub trait IGatewayClientV2: Debug + Send + Sync {
    /// Use the gateway's lightning node to complete a payment
    async fn complete_htlc(&self, htlc_response: InterceptPaymentResponse);

    /// Try to settle an outgoing payment via a direct swap to another
    /// federation hosted by the same gateway. If the gateway's connected
    /// lightning node is the invoice's payee the gateway dispatches the swap
    /// against the target federation's `GatewayClientModuleV2` and returns
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

// --- Types shared with fedimint-gateway-daemon ---

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
    Forward,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct Preimage(pub [u8; 32]);

impl std::fmt::Display for Preimage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.encode_hex::<String>())
    }
}
