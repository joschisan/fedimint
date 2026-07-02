pub mod api;
mod complete_sm;
pub mod events;
mod receive_sm;
mod send_sm;

use std::collections::BTreeMap;
use std::fmt;
use std::fmt::Debug;
use std::sync::Arc;

use anyhow::{anyhow, ensure};
use async_trait::async_trait;
use bitcoin::hashes::sha256;
use bitcoin::secp256k1::Message;
use events::{
    IncomingPaymentStarted, OutgoingPaymentFailed, OutgoingPaymentStarted, OutgoingPaymentSucceeded,
};
use fedimint_api_client::api::DynModuleApi;
use fedimint_client::ClientHandleArc;
use fedimint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client_module::module::recovery::NoModuleBackup;
use fedimint_client_module::module::{ClientContext, ClientModule, IClientModule, OutPointRange};
use fedimint_client_module::sm::{Context, DynState, ModuleNotifier, State, StateTransition};
use fedimint_client_module::transaction::{
    ClientInput, ClientInputBundle, ClientOutput, ClientOutputBundle, ClientOutputSM,
    TransactionBuilder,
};
use fedimint_client_module::{DynGlobalClientContext, sm_enum_variant_translation};
use fedimint_core::config::FederationId;
use fedimint_core::core::{Decoder, IntoDynInstance, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::DatabaseTransaction;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{
    Amounts, ApiVersion, CommonModuleInit, ModuleCommon, ModuleInit, MultiApiVersion,
};
use fedimint_core::secp256k1::Keypair;
use fedimint_core::time::now;
use fedimint_core::util::Spanned;
use fedimint_core::{Amount, OutPoint, PeerId, apply, async_trait_maybe_send, secp256k1};
use fedimint_lightning::{InterceptPaymentResponse, LightningRpcError};
use fedimint_lnv2_common::config::LightningClientConfig;
use fedimint_lnv2_common::contracts::{IncomingContract, OutgoingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::SendPaymentPayload;
use fedimint_lnv2_common::{
    LightningCommonInit, LightningInput, LightningInputV0, LightningInvoice, LightningModuleTypes,
    LightningOutput, LightningOutputV0, OutgoingWitness,
};
use futures::StreamExt;
use lightning_invoice::Bolt11Invoice;
use receive_sm::{ReceiveSMState, ReceiveStateMachine};
use secp256k1::schnorr::Signature;
use send_sm::{SendSMState, SendStateMachine};
use serde::{Deserialize, Serialize};
use tpe::{AggregatePublicKey, PublicKeyShare};
use tracing::{info, warn};

use crate::api::GatewayFederationApi;
use crate::complete_sm::{CompleteSMCommon, CompleteSMState, CompleteStateMachine};
use crate::receive_sm::ReceiveSMCommon;
use crate::send_sm::{Cancelled, SendSMCommon};

/// LNv2 CLTV Delta in blocks
pub const EXPIRATION_DELTA_MINIMUM_V2: u64 = 144;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GatewayOperationMetaV2;

#[derive(Debug, Clone)]
pub struct GatewayClientInitV2 {
    /// Daemon-side callback interface used by the Send and Complete state
    /// machines and by [`GatewayClientModuleV2::send_payment`]. Only the v1
    /// gateway drives those paths and it always provides the callback; the v2
    /// gateway orchestrates payments in the daemon itself and passes `None`.
    pub gateway: Option<Arc<dyn IGatewayClientV2>>,
}

impl ModuleInit for GatewayClientInitV2 {
    type Common = LightningCommonInit;

    async fn dump_database(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _prefix_names: Vec<String>,
    ) -> Box<dyn Iterator<Item = (String, Box<dyn erased_serde::Serialize + Send>)> + '_> {
        Box::new(vec![].into_iter())
    }
}

#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for GatewayClientInitV2 {
    type Module = GatewayClientModuleV2;

    fn supported_api_versions(&self) -> MultiApiVersion {
        MultiApiVersion::try_from_iter([ApiVersion { major: 0, minor: 0 }])
            .expect("no version conflicts")
    }

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(GatewayClientModuleV2 {
            federation_id: *args.federation_id(),
            cfg: args.cfg().clone(),
            notifier: args.notifier().clone(),
            client_ctx: args.context(),
            module_api: args.module_api().clone(),
            keypair: args
                .module_root_secret()
                .clone()
                .to_secp_key(fedimint_core::secp256k1::SECP256K1),
            gateway: self.gateway.clone(),
        })
    }
}

#[derive(Debug, Clone)]
pub struct GatewayClientModuleV2 {
    pub federation_id: FederationId,
    pub cfg: LightningClientConfig,
    pub notifier: ModuleNotifier<GatewayClientStateMachinesV2>,
    pub client_ctx: ClientContext<Self>,
    pub module_api: DynModuleApi,
    pub keypair: Keypair,
    pub gateway: Option<Arc<dyn IGatewayClientV2>>,
}

impl GatewayClientModuleV2 {
    /// The daemon-side callback interface. See
    /// [`GatewayClientInitV2::gateway`] for why this is only available on the
    /// v1 gateway.
    fn gateway(&self) -> &Arc<dyn IGatewayClientV2> {
        self.gateway.as_ref().expect(
            "only the v1 gateway calls back into the daemon and it always provides the callback",
        )
    }
}

#[derive(Debug, Clone)]
pub struct GatewayClientContextV2 {
    pub module: GatewayClientModuleV2,
    pub decoder: Decoder,
    pub tpe_agg_pk: AggregatePublicKey,
    pub tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
    pub gateway: Option<Arc<dyn IGatewayClientV2>>,
}

impl GatewayClientContextV2 {
    /// The daemon-side callback interface. See
    /// [`GatewayClientInitV2::gateway`] for why this is only available on the
    /// v1 gateway.
    fn gateway(&self) -> &Arc<dyn IGatewayClientV2> {
        self.gateway
            .as_ref()
            .expect("only the v1 gateway spawns the state machines that call back into the daemon and it always provides the callback")
    }
}

impl Context for GatewayClientContextV2 {
    const KIND: Option<ModuleKind> = Some(fedimint_lnv2_common::KIND);
}

impl ClientModule for GatewayClientModuleV2 {
    type Init = GatewayClientInitV2;
    type Common = LightningModuleTypes;
    type Backup = NoModuleBackup;
    type ModuleStateMachineContext = GatewayClientContextV2;
    type States = GatewayClientStateMachinesV2;

    fn context(&self) -> Self::ModuleStateMachineContext {
        GatewayClientContextV2 {
            module: self.clone(),
            decoder: self.decoder(),
            tpe_agg_pk: self.cfg.tpe_agg_pk,
            tpe_pks: self.cfg.tpe_pks.clone(),
            gateway: self.gateway.clone(),
        }
    }
    fn input_fee(
        &self,
        amount: &Amounts,
        _input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amounts> {
        Some(Amounts::new_bitcoin(
            self.cfg.fee_consensus.fee(amount.expect_only_bitcoin()),
        ))
    }

    fn output_fee(
        &self,
        _amount: &Amounts,
        output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amounts> {
        let amount = match output.ensure_v0_ref().ok()? {
            LightningOutputV0::Outgoing(contract) => contract.amount,
            LightningOutputV0::Incoming(contract) => contract.commitment.amount,
        };

        Some(Amounts::new_bitcoin(self.cfg.fee_consensus.fee(amount)))
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum GatewayClientStateMachinesV2 {
    Send(SendStateMachine),
    Receive(ReceiveStateMachine),
    Complete(CompleteStateMachine),
}

impl fmt::Display for GatewayClientStateMachinesV2 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GatewayClientStateMachinesV2::Send(send) => {
                write!(f, "{send}")
            }
            GatewayClientStateMachinesV2::Receive(receive) => {
                write!(f, "{receive}")
            }
            GatewayClientStateMachinesV2::Complete(complete) => {
                write!(f, "{complete}")
            }
        }
    }
}

impl IntoDynInstance for GatewayClientStateMachinesV2 {
    type DynType = DynState;

    fn into_dyn(self, instance_id: ModuleInstanceId) -> Self::DynType {
        DynState::from_typed(instance_id, self)
    }
}

impl State for GatewayClientStateMachinesV2 {
    type ModuleContext = GatewayClientContextV2;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        match self {
            GatewayClientStateMachinesV2::Send(state) => {
                sm_enum_variant_translation!(
                    state.transitions(context, global_context),
                    GatewayClientStateMachinesV2::Send
                )
            }
            GatewayClientStateMachinesV2::Receive(state) => {
                sm_enum_variant_translation!(
                    state.transitions(context, global_context),
                    GatewayClientStateMachinesV2::Receive
                )
            }
            GatewayClientStateMachinesV2::Complete(state) => {
                sm_enum_variant_translation!(
                    state.transitions(context, global_context),
                    GatewayClientStateMachinesV2::Complete
                )
            }
        }
    }

    fn operation_id(&self) -> OperationId {
        match self {
            GatewayClientStateMachinesV2::Send(state) => state.operation_id(),
            GatewayClientStateMachinesV2::Receive(state) => state.operation_id(),
            GatewayClientStateMachinesV2::Complete(state) => state.operation_id(),
        }
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
        let operation_start = now();

        // The operation id is equal to the contract id which also doubles as the
        // message signed by the gateway via the forfeit signature to forfeit
        // the gateways claim to a contract in case of cancellation. We only create a
        // forfeit signature after we have started the send state machine to
        // prevent replay attacks with a previously cancelled outgoing contract
        let operation_id = OperationId::from_encodable(&payload.contract.clone());

        if self.client_ctx.operation_exists(operation_id).await {
            return Ok(self.subscribe_send(operation_id).await);
        }

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
            .gateway()
            .min_contract_amount(&payload.federation_id, amount)
            .await?;

        let send_sm = GatewayClientStateMachinesV2::Send(SendStateMachine {
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
        });

        let mut dbtx = self.client_ctx.module_db().begin_transaction().await;
        self.client_ctx
            .manual_operation_start_dbtx(
                &mut dbtx.to_ref_nc(),
                operation_id,
                LightningCommonInit::KIND.as_str(),
                GatewayOperationMetaV2,
                vec![self.client_ctx.make_dyn_state(send_sm)],
            )
            .await
            .ok();

        self.client_ctx
            .log_event(
                &mut dbtx,
                OutgoingPaymentStarted {
                    operation_start,
                    outgoing_contract: payload.contract.clone(),
                    min_contract_amount,
                    invoice_amount: Amount::from_msats(amount),
                    max_delay: expiration.saturating_sub(EXPIRATION_DELTA_MINIMUM_V2),
                },
            )
            .await;
        dbtx.commit_tx().await;

        Ok(self.subscribe_send(operation_id).await)
    }

    pub async fn subscribe_send(&self, operation_id: OperationId) -> Result<[u8; 32], Signature> {
        let mut stream = self.notifier.subscribe(operation_id).await;

        loop {
            if let Some(GatewayClientStateMachinesV2::Send(state)) = stream.next().await {
                match state.state {
                    SendSMState::Sending => {}
                    SendSMState::Claiming(claiming) => {
                        // The preimage is proof the payment succeeded, so return it to
                        // the sender as soon as it is available rather than waiting for
                        // an additional ordering. The gateway's claim of the outgoing
                        // contract has already been submitted by the send state machine
                        // and finalizes in the background.
                        return Ok(claiming.preimage);
                    }
                    SendSMState::Cancelled(cancelled) => {
                        warn!("Outgoing lightning payment is cancelled {:?}", cancelled);

                        let signature = self
                            .keypair
                            .sign_schnorr(state.common.contract.forfeit_message());

                        assert!(state.common.contract.verify_forfeit_signature(&signature));

                        return Err(signature);
                    }
                }
            }
        }
    }

    pub async fn relay_incoming_htlc(
        &self,
        payment_hash: sha256::Hash,
        incoming_chan_id: u64,
        htlc_id: u64,
        contract: IncomingContract,
        amount_msat: u64,
    ) -> anyhow::Result<()> {
        let operation_start = now();

        let operation_id = OperationId::from_encodable(&contract);

        if self.client_ctx.operation_exists(operation_id).await {
            return Ok(());
        }

        let refund_keypair = self.keypair;

        let client_output = ClientOutput::<LightningOutput> {
            output: LightningOutput::V0(LightningOutputV0::Incoming(contract.clone())),
            amounts: Amounts::new_bitcoin(contract.commitment.amount),
        };
        let commitment = contract.commitment.clone();
        let client_output_sm = ClientOutputSM::<GatewayClientStateMachinesV2> {
            state_machines: Arc::new(move |range: OutPointRange| {
                assert_eq!(range.count(), 1);

                vec![
                    GatewayClientStateMachinesV2::Receive(ReceiveStateMachine {
                        common: ReceiveSMCommon {
                            operation_id,
                            contract: contract.clone(),
                            outpoint: range.into_iter().next().unwrap(),
                            refund_keypair,
                        },
                        state: ReceiveSMState::Funding,
                    }),
                    GatewayClientStateMachinesV2::Complete(CompleteStateMachine {
                        common: CompleteSMCommon {
                            operation_id,
                            payment_hash,
                            incoming_chan_id,
                            htlc_id,
                        },
                        state: CompleteSMState::Pending,
                    }),
                ]
            }),
        };

        let client_output = self.client_ctx.make_client_outputs(ClientOutputBundle::new(
            vec![client_output],
            vec![client_output_sm],
        ));
        let transaction = TransactionBuilder::new().with_outputs(client_output);

        self.client_ctx
            .finalize_and_submit_transaction(
                operation_id,
                LightningCommonInit::KIND.as_str(),
                |_| GatewayOperationMetaV2,
                transaction,
            )
            .await?;

        let mut dbtx = self.client_ctx.module_db().begin_transaction().await;
        self.client_ctx
            .log_event(
                &mut dbtx,
                IncomingPaymentStarted {
                    operation_start,
                    incoming_contract_commitment: commitment,
                    invoice_amount: Amount::from_msats(amount_msat),
                },
            )
            .await;
        dbtx.commit_tx().await;

        Ok(())
    }

    /// Funds the incoming contract and spawns **only** the
    /// [`ReceiveStateMachine`] (no [`CompleteStateMachine`]), fire-and-forget.
    /// Returns the operation id immediately without awaiting the terminal
    /// state; the upstream Lightning HTLC (for external receives) is settled
    /// out-of-band by the daemon-side receive trailer, which tails this
    /// federation's event log for [`IncomingPaymentSucceeded`].
    ///
    /// Idempotent on `operation`: a retry with the same contract is a no-op and
    /// just returns the existing operation id.
    pub async fn start_receive(
        &self,
        operation_id: OperationId,
        contract: IncomingContract,
        amount_msat: u64,
    ) -> anyhow::Result<OperationId> {
        let operation_start = now();

        if self.client_ctx.operation_exists(operation_id).await {
            return Ok(operation_id);
        }

        let refund_keypair = self.keypair;

        let client_output = ClientOutput::<LightningOutput> {
            output: LightningOutput::V0(LightningOutputV0::Incoming(contract.clone())),
            amounts: Amounts::new_bitcoin(contract.commitment.amount),
        };
        let commitment = contract.commitment.clone();
        let client_output_sm = ClientOutputSM::<GatewayClientStateMachinesV2> {
            state_machines: Arc::new(move |range| {
                assert_eq!(range.count(), 1);

                vec![GatewayClientStateMachinesV2::Receive(ReceiveStateMachine {
                    common: ReceiveSMCommon {
                        operation_id,
                        contract: contract.clone(),
                        outpoint: range.into_iter().next().unwrap(),
                        refund_keypair,
                    },
                    state: ReceiveSMState::Funding,
                })]
            }),
        };

        let client_output = self.client_ctx.make_client_outputs(ClientOutputBundle::new(
            vec![client_output],
            vec![client_output_sm],
        ));

        let transaction = TransactionBuilder::new().with_outputs(client_output);

        self.client_ctx
            .finalize_and_submit_transaction(
                operation_id,
                LightningCommonInit::KIND.as_str(),
                |_| GatewayOperationMetaV2,
                transaction,
            )
            .await?;

        let mut dbtx = self.client_ctx.module_db().begin_transaction().await;
        self.client_ctx
            .log_event_for_operation(
                &mut dbtx,
                operation_id,
                IncomingPaymentStarted {
                    operation_start,
                    incoming_contract_commitment: commitment,
                    invoice_amount: Amount::from_msats(amount_msat),
                },
            )
            .await;
        dbtx.commit_tx().await;

        Ok(operation_id)
    }

    pub async fn relay_direct_swap(
        &self,
        contract: IncomingContract,
        amount_msat: u64,
    ) -> anyhow::Result<FinalReceiveState> {
        let operation_id = OperationId::from_encodable(&contract);

        let operation_id = self
            .start_receive(operation_id, contract, amount_msat)
            .await?;

        Ok(self.await_receive(operation_id).await)
    }

    pub async fn await_receive(&self, operation_id: OperationId) -> FinalReceiveState {
        let mut stream = self.notifier.subscribe(operation_id).await;

        loop {
            if let Some(GatewayClientStateMachinesV2::Receive(state)) = stream.next().await {
                match state.state {
                    ReceiveSMState::Funding => {}
                    ReceiveSMState::Rejected(..) => return FinalReceiveState::Rejected,
                    ReceiveSMState::Success(preimage) => {
                        return FinalReceiveState::Success(preimage);
                    }
                    ReceiveSMState::Refunding(out_points) => {
                        if self
                            .client_ctx
                            .await_primary_module_outputs(operation_id, out_points)
                            .await
                            .is_err()
                        {
                            return FinalReceiveState::Failure;
                        }

                        return FinalReceiveState::Refunded;
                    }
                    ReceiveSMState::Failure => return FinalReceiveState::Failure,
                }
            }
        }
    }

    /// For the given `OperationId`, this function will wait until the Complete
    /// state machine has finished or failed.
    pub async fn await_completion(&self, operation_id: OperationId) {
        let mut stream = self.notifier.subscribe(operation_id).await;

        loop {
            match stream.next().await {
                Some(GatewayClientStateMachinesV2::Complete(state)) => {
                    if state.state == CompleteSMState::Completed {
                        info!(%state, "LNv2 completion state machine finished");
                        return;
                    }

                    info!(%state, "Waiting for LNv2 completion state machine");
                }
                Some(GatewayClientStateMachinesV2::Receive(state)) => {
                    info!(%state, "Waiting for LNv2 completion state machine");
                    continue;
                }
                Some(state) => {
                    warn!(%state, "Operation is not an LNv2 completion state machine");
                    return;
                }
                None => return,
            }
        }
    }

    /// Logs the [`OutgoingPaymentStarted`] event for a daemon-driven send,
    /// operation-tagged so the send can be observed via
    /// `subscribe_operation_events`. Unlike [`Self::send_payment`] this spawns
    /// no [`SendStateMachine`]: the daemon owns the outgoing payment and calls
    /// [`Self::finalize_send`] once it learns the outcome.
    pub async fn log_send_started(
        &self,
        operation_id: OperationId,
        contract: OutgoingContract,
        min_contract_amount: Amount,
        invoice_amount: Amount,
        max_delay: u64,
    ) {
        let mut dbtx = self.client_ctx.module_db().begin_transaction().await;
        self.client_ctx
            .log_event_for_operation(
                &mut dbtx,
                operation_id,
                OutgoingPaymentStarted {
                    operation_start: now(),
                    outgoing_contract: contract,
                    min_contract_amount,
                    invoice_amount,
                    max_delay,
                },
            )
            .await;
        dbtx.commit_tx().await;
    }

    /// Terminal work for a daemon-driven outgoing contract, replacing the
    /// [`SendStateMachine`]'s claim/forfeit transition:
    ///
    /// - `Some(preimage)` claims the outgoing contract (submits an
    ///   [`OutgoingWitness::Claim`] input) and logs
    ///   [`OutgoingPaymentSucceeded`].
    /// - `None` signs the forfeit message, logs [`OutgoingPaymentFailed`], and
    ///   returns the forfeit [`Signature`] so the caller can return it to the
    ///   sending client.
    ///
    /// Operation-tagged so the terminal is observable via
    /// `subscribe_operation_events`.
    pub async fn finalize_send(
        &self,
        operation_id: OperationId,
        contract: OutgoingContract,
        outpoint: OutPoint,
        preimage: Option<[u8; 32]>,
        ln_fee: Amount,
    ) -> anyhow::Result<Option<Signature>> {
        match preimage {
            Some(preimage) => {
                let client_input = ClientInput::<LightningInput> {
                    input: LightningInput::V0(LightningInputV0::Outgoing(
                        outpoint,
                        OutgoingWitness::Claim(preimage),
                    )),
                    amounts: Amounts::new_bitcoin(contract.amount),
                    keys: vec![self.keypair],
                };

                let inputs = self
                    .client_ctx
                    .make_client_inputs(ClientInputBundle::new_no_sm(vec![client_input]));

                self.client_ctx
                    .finalize_and_submit_transaction(
                        operation_id,
                        LightningCommonInit::KIND.as_str(),
                        |_| GatewayOperationMetaV2,
                        TransactionBuilder::new().with_inputs(inputs),
                    )
                    .await?;

                let mut dbtx = self.client_ctx.module_db().begin_transaction().await;
                self.client_ctx
                    .log_event_for_operation(
                        &mut dbtx,
                        operation_id,
                        OutgoingPaymentSucceeded {
                            payment_image: contract.payment_image.clone(),
                            target_federation: None,
                            preimage: Some(preimage),
                            ln_fee: Some(ln_fee),
                        },
                    )
                    .await;
                dbtx.commit_tx().await;

                Ok(None)
            }
            None => {
                let signature = self.keypair.sign_schnorr(contract.forfeit_message());

                let mut dbtx = self.client_ctx.module_db().begin_transaction().await;
                self.client_ctx
                    .log_event_for_operation(
                        &mut dbtx,
                        operation_id,
                        OutgoingPaymentFailed {
                            payment_image: contract.payment_image.clone(),
                            error: Cancelled::Failure,
                            forfeit_signature: Some(signature),
                        },
                    )
                    .await;
                dbtx.commit_tx().await;

                Ok(Some(signature))
            }
        }
    }
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

    /// Determines if the payment can be completed using a direct swap to
    /// another federation.
    ///
    /// A direct swap is determined by checking the gateway's connected
    /// lightning node against the invoice's payee lightning node. If they
    /// are the same, then the gateway can use another client to complete
    /// the payment be swapping ecash instead of a payment over the
    /// Lightning network.
    async fn is_direct_swap(
        &self,
        invoice: &Bolt11Invoice,
    ) -> anyhow::Result<Option<(IncomingContract, ClientHandleArc)>>;

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

    /// Check if this invoice was created using LNv1 and if the gateway is
    /// connected to the target federation.
    async fn is_lnv1_invoice(&self, invoice: &Bolt11Invoice) -> Option<Spanned<ClientHandleArc>>;

    /// Perform a swap from an LNv2 `OutgoingContract` to an LNv1
    /// `IncomingContract`
    async fn relay_lnv1_swap(
        &self,
        client: &ClientHandleArc,
        invoice: &Bolt11Invoice,
    ) -> anyhow::Result<FinalReceiveState>;
}
