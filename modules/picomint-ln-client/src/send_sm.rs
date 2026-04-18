use anyhow::ensure;
use bitcoin::hashes::sha256;
use futures::future::pending;
use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_client_module::transaction::{ClientInput, ClientInputBundle};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::util::SafeUrl;
use picomint_core::backoff::{Retryable, networking_backoff};
use picomint_core::{OutPoint, secp256k1};
use picomint_ln_common::contracts::OutgoingContract;
use picomint_ln_common::{LightningInput, OutgoingWitness};
use picomint_logging::LOG_CLIENT_MODULE_LN;
use picomint_redb::WriteTxRef;
use secp256k1::Keypair;
use secp256k1::schnorr::Signature;
use tracing::{error, instrument};

use crate::api::LightningFederationApi;
use crate::events::{SendPaymentStatus, SendPaymentUpdateEvent};
use crate::{LightningClientContext, LightningInvoice};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendStateMachine {
    pub common: SendSMCommon,
    pub state: SendSMState,
}

picomint_redb::consensus_value!(SendStateMachine);

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
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that requests the lightning gateway to pay an invoice on
/// behalf of a federation client.

async fn send_update_event_sm(
    ctx: &LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    operation_id: OperationId,
    status: SendPaymentStatus,
) {
    ctx.client_ctx
        .log_event(dbtx, operation_id, SendPaymentUpdateEvent { status })
        .await;
}

impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = LightningClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            SendSMState::Funding => {
                let ctx_clone = ctx.clone();
                let operation_id = self.common.operation_id;
                let txid = self.common.outpoint.txid;
                vec![SmStateTransition::new(
                    async move {
                        ctx_clone
                            .client_ctx
                            .await_tx_accepted(operation_id, txid)
                            .await
                    },
                    |_dbtx: &WriteTxRef<'_>,
                     result: Result<(), String>,
                     old_state: SendStateMachine| {
                        Box::pin(async move {
                            match result {
                                Ok(()) => Some(old_state.update(SendSMState::Funded)),
                                Err(_) => None,
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
        }
    }
}

#[instrument(target = LOG_CLIENT_MODULE_LN, skip(refund_keypair, ctx))]
async fn gateway_send_payment_sm(
    gateway_api: SafeUrl,
    federation_id: FederationId,
    outpoint: OutPoint,
    contract: OutgoingContract,
    invoice: LightningInvoice,
    refund_keypair: Keypair,
    ctx: LightningClientContext,
) -> Result<[u8; 32], Signature> {
    (|| async {
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
    .retry(networking_backoff())
    .await
    .expect("networking_backoff retries forever")
}

async fn transition_gateway_send_payment_sm(
    ctx: LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    gateway_response: Result<[u8; 32], Signature>,
    old_state: SendStateMachine,
) -> Option<SendStateMachine> {
    match gateway_response {
        Ok(preimage) => {
            send_update_event_sm(
                &ctx,
                dbtx,
                old_state.common.operation_id,
                SendPaymentStatus::Success(preimage),
            )
            .await;
        }
        Err(signature) => {
            let client_input = ClientInput::<LightningInput> {
                input: LightningInput::Outgoing(
                    old_state.common.outpoint,
                    OutgoingWitness::Cancel(signature),
                ),
                amount: old_state.common.contract.amount,
                keys: vec![old_state.common.refund_keypair],
            };

            ctx.client_ctx
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
        }
    }

    None
}

#[instrument(target = LOG_CLIENT_MODULE_LN, skip(ctx))]
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

    error!(target: LOG_CLIENT_MODULE_LN, "Federation returned invalid preimage {:?}", preimage);

    pending().await
}

async fn transition_preimage_sm(
    ctx: LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    old_state: SendStateMachine,
    preimage: Option<[u8; 32]>,
) -> Option<SendStateMachine> {
    if let Some(preimage) = preimage {
        send_update_event_sm(
            &ctx,
            dbtx,
            old_state.common.operation_id,
            SendPaymentStatus::Success(preimage),
        )
        .await;

        return None;
    }

    let client_input = ClientInput::<LightningInput> {
        input: LightningInput::Outgoing(old_state.common.outpoint, OutgoingWitness::Refund),
        amount: old_state.common.contract.amount,
        keys: vec![old_state.common.refund_keypair],
    };

    ctx.client_ctx
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

    None
}
