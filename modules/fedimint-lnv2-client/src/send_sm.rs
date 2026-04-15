use anyhow::ensure;
use bitcoin::hashes::sha256;
use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::util::SafeUrl;
use fedimint_core::util::backoff_util::api_networking_backoff;
use fedimint_core::{OutPoint, crit, secp256k1, util};
use fedimint_lnv2_common::contracts::OutgoingContract;
use fedimint_lnv2_common::{LightningInput, LightningInputV0, OutgoingWitness};
use fedimint_logging::LOG_CLIENT_MODULE_LNV2;
use fedimint_redb::v2::WriteTxRef;
use futures::future::pending;
use secp256k1::Keypair;
use secp256k1::schnorr::Signature;
use tracing::instrument;

use crate::api::LightningFederationApi;
use crate::events::{SendPaymentStatus, SendPaymentUpdateEvent};
use crate::{LightningClientContext, LightningInvoice};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendStateMachine {
    pub common: SendSMCommon,
    pub state: SendSMState,
}

impl SendStateMachine {
    pub fn update(&self, state: SendSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendSMCommon {
    pub operation_id: OperationId,
    pub outpoint: OutPoint,
    pub contract: OutgoingContract,
    pub gateway_api: Option<SafeUrl>,
    pub invoice: Option<LightningInvoice>,
    pub refund_keypair: Keypair,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum SendSMState {
    Funding,
    Funded,
    Rejected(String),
    Success([u8; 32]),
    Refunding(Vec<OutPoint>),
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that requests the lightning gateway to pay an invoice on
/// behalf of a federation client.
///
/// ```mermaid
/// graph LR
/// classDef virtual fill:#fff,stroke-dasharray: 5 5
///
///     Funding -- funding tx is rejected --> Rejected
///     Funding -- funding tx is accepted --> Funded
///     Funded -- post invoice returns preimage  --> Success
///     Funded -- post invoice returns forfeit tx --> Refunding
///     Funded -- await_preimage returns preimage --> Success
///     Funded -- await_preimage expires --> Refunding
/// ```

async fn send_update_event_sm(
    ctx: &LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    operation_id: OperationId,
    status: SendPaymentStatus,
) {
    ctx.client_ctx
        .log_event(
            dbtx,
            SendPaymentUpdateEvent {
                operation_id,
                status,
            },
        )
        .await;
}

impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = LightningClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            SendSMState::Funding => {
                let ctx_clone = ctx.clone();
                let txid = self.common.outpoint.txid;
                vec![SmStateTransition::new(
                    async move { ctx_clone.client_ctx.await_tx_accepted(txid).await },
                    |_dbtx, result: Result<(), String>, old_state: SendStateMachine| {
                        Box::pin(async move {
                            match result {
                                Ok(()) => old_state.update(SendSMState::Funded),
                                Err(error) => old_state.update(SendSMState::Rejected(error)),
                            }
                        })
                    },
                )]
            }
            SendSMState::Funded => {
                let c_pay = ctx.clone();
                let c_preimage = ctx.clone();
                let outpoint = self.common.outpoint;
                let contract = self.common.contract.clone();
                let gateway_api = self.common.gateway_api.clone().unwrap();
                let invoice = self.common.invoice.clone().unwrap();
                let refund_keypair = self.common.refund_keypair;
                let fed_id = ctx.federation_id;

                vec![
                    SmStateTransition::new(
                        gateway_send_payment_sm(
                            gateway_api,
                            fed_id,
                            outpoint,
                            contract.clone(),
                            invoice,
                            refund_keypair,
                            c_pay.clone(),
                        ),
                        move |dbtx, response, old_state| {
                            let ctx = c_pay.clone();
                            Box::pin(transition_gateway_send_payment_sm(
                                ctx, dbtx, response, old_state,
                            ))
                        },
                    ),
                    SmStateTransition::new(
                        await_preimage_sm(outpoint, contract.clone(), c_preimage.clone()),
                        move |dbtx, preimage, old_state| {
                            let ctx = c_preimage.clone();
                            Box::pin(transition_preimage_sm(ctx, dbtx, old_state, preimage))
                        },
                    ),
                ]
            }
            SendSMState::Refunding(..) | SendSMState::Success(..) | SendSMState::Rejected(..) => {
                vec![]
            }
        }
    }
}

#[instrument(target = LOG_CLIENT_MODULE_LNV2, skip(refund_keypair, ctx))]
async fn gateway_send_payment_sm(
    gateway_api: SafeUrl,
    federation_id: FederationId,
    outpoint: OutPoint,
    contract: OutgoingContract,
    invoice: LightningInvoice,
    refund_keypair: Keypair,
    ctx: LightningClientContext,
) -> Result<[u8; 32], Signature> {
    util::retry("gateway-send-payment", api_networking_backoff(), || async {
        let payment_result = ctx
            .gateway_conn
            .send_payment(
                gateway_api.clone(),
                federation_id,
                outpoint,
                contract.clone(),
                invoice.clone(),
                refund_keypair.sign_schnorr(secp256k1::Message::from_digest(
                    *invoice.consensus_hash::<sha256::Hash>().as_ref(),
                )),
            )
            .await?;

        ensure!(
            contract.verify_gateway_response(&payment_result),
            "Invalid gateway response: {payment_result:?}"
        );

        Ok(payment_result)
    })
    .await
    .expect("Number of retries has no limit")
}

async fn transition_gateway_send_payment_sm(
    ctx: LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    gateway_response: Result<[u8; 32], Signature>,
    old_state: SendStateMachine,
) -> SendStateMachine {
    match gateway_response {
        Ok(preimage) => {
            send_update_event_sm(
                &ctx,
                dbtx,
                old_state.common.operation_id,
                SendPaymentStatus::Success(preimage),
            )
            .await;

            old_state.update(SendSMState::Success(preimage))
        }
        Err(signature) => {
            let client_input = ClientInput::<LightningInput> {
                input: LightningInput::V0(LightningInputV0::Outgoing(
                    old_state.common.outpoint,
                    OutgoingWitness::Cancel(signature),
                )),
                amount: old_state.common.contract.amount,
                keys: vec![old_state.common.refund_keypair],
            };

            let change_range = ctx
                .client_ctx
                .claim_inputs(
                    dbtx,
                    ClientInputBundle::new(vec![client_input]),
                    old_state.common.operation_id,
                )
                .await
                .expect("Cannot claim input, additional funding needed");

            send_update_event_sm(
                &ctx,
                dbtx,
                old_state.common.operation_id,
                SendPaymentStatus::Refunded,
            )
            .await;

            old_state.update(SendSMState::Refunding(change_range.into_iter().collect()))
        }
    }
}

#[instrument(target = LOG_CLIENT_MODULE_LNV2, skip(ctx))]
async fn await_preimage_sm(
    outpoint: OutPoint,
    contract: OutgoingContract,
    ctx: LightningClientContext,
) -> Option<[u8; 32]> {
    let preimage = ctx
        .client_ctx
        .module_api()
        .await_preimage(outpoint, contract.expiration)
        .await?;

    if contract.verify_preimage(&preimage) {
        return Some(preimage);
    }

    crit!(target: LOG_CLIENT_MODULE_LNV2, "Federation returned invalid preimage {:?}", preimage);

    pending().await
}

async fn transition_preimage_sm(
    ctx: LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    old_state: SendStateMachine,
    preimage: Option<[u8; 32]>,
) -> SendStateMachine {
    if let Some(preimage) = preimage {
        send_update_event_sm(
            &ctx,
            dbtx,
            old_state.common.operation_id,
            SendPaymentStatus::Success(preimage),
        )
        .await;

        return old_state.update(SendSMState::Success(preimage));
    }

    let client_input = ClientInput::<LightningInput> {
        input: LightningInput::V0(LightningInputV0::Outgoing(
            old_state.common.outpoint,
            OutgoingWitness::Refund,
        )),
        amount: old_state.common.contract.amount,
        keys: vec![old_state.common.refund_keypair],
    };

    let change_range = ctx
        .client_ctx
        .claim_inputs(
            dbtx,
            ClientInputBundle::new(vec![client_input]),
            old_state.common.operation_id,
        )
        .await
        .expect("Cannot claim input, additional funding needed");

    send_update_event_sm(
        &ctx,
        dbtx,
        old_state.common.operation_id,
        SendPaymentStatus::Refunded,
    )
    .await;

    old_state.update(SendSMState::Refunding(change_range.into_iter().collect()))
}
