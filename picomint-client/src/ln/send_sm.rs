use crate::executor::StateMachine;
use crate::transaction::{Input, TransactionBuilder};
use picomint_core::wire;
use anyhow::ensure;
use bitcoin::hashes::sha256;
use futures::future::pending;
use picomint_core::backoff::{Retryable, networking_backoff};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::ln::contracts::OutgoingContract;
use picomint_core::ln::{LightningInput, OutgoingWitness};
use picomint_core::util::SafeUrl;
use picomint_core::{OutPoint, secp256k1};
use picomint_encoding::{Decodable, Encodable};
use picomint_logging::LOG_CLIENT_MODULE_LN;
use picomint_redb::WriteTxRef;
use secp256k1::Keypair;
use secp256k1::schnorr::Signature;
use tracing::{error, instrument};

use super::events::{SendRefundEvent, SendSuccessEvent};
use super::{LightningClientContext, LightningInvoice};

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

/// Outcome produced by [`SendStateMachine::trigger`]. Which variant is
/// yielded depends on the current [`SendSMState`]:
/// - `Funding`  → [`SendOutcome::FundingResult`]
/// - `Funded`   → [`SendOutcome::GatewayResponse`] or [`SendOutcome::Preimage`],
///   whichever of the two races finishes first.
pub enum SendOutcome {
    FundingResult(Result<(), String>),
    GatewayResponse(Result<[u8; 32], Signature>),
    Preimage(Option<[u8; 32]>),
}

/// State machine that requests the lightning gateway to pay an invoice on
/// behalf of a federation client.
impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "ln-send-sm";

    type Context = LightningClientContext;
    type Outcome = SendOutcome;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        match &self.state {
            SendSMState::Funding => SendOutcome::FundingResult(
                ctx.client_ctx
                    .await_tx_accepted(self.common.operation_id, self.common.outpoint.txid)
                    .await,
            ),
            SendSMState::Funded => {
                let gateway_api = self.common.gateway_api.clone().unwrap();
                let invoice = self.common.invoice.clone().unwrap();
                tokio::select! {
                    response = gateway_send_payment_sm(
                        gateway_api,
                        ctx.federation_id,
                        self.common.outpoint,
                        self.common.contract.clone(),
                        invoice,
                        self.common.refund_keypair,
                    ) => SendOutcome::GatewayResponse(response),
                    preimage = await_preimage_sm(
                        self.common.outpoint,
                        self.common.contract.clone(),
                        ctx.clone(),
                    ) => SendOutcome::Preimage(preimage),
                }
            }
        }
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        match outcome {
            SendOutcome::FundingResult(Ok(())) => Some(self.update(SendSMState::Funded)),
            SendOutcome::FundingResult(Err(_)) => None,
            SendOutcome::GatewayResponse(response) => {
                transition_gateway_send_payment_sm(ctx, dbtx, self, response).await;
                None
            }
            SendOutcome::Preimage(preimage) => {
                transition_preimage_sm(ctx, dbtx, self, preimage).await;
                None
            }
        }
    }
}

#[instrument(target = LOG_CLIENT_MODULE_LN, skip(refund_keypair))]
async fn gateway_send_payment_sm(
    gateway_api: SafeUrl,
    federation_id: FederationId,
    outpoint: OutPoint,
    contract: OutgoingContract,
    invoice: LightningInvoice,
    refund_keypair: Keypair,
) -> Result<[u8; 32], Signature> {
    (|| async {
        let payment_result = crate::ln::gateway_http::send_payment(
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
    ctx: &LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    old_state: &SendStateMachine,
    gateway_response: Result<[u8; 32], Signature>,
) {
    match gateway_response {
        Ok(preimage) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    SendSuccessEvent { preimage },
                )
                .await;
        }
        Err(signature) => {
            let tx_builder = TransactionBuilder::from_input(Input {
                input: wire::Input::Ln(LightningInput::Outgoing(
                    old_state.common.outpoint,
                    OutgoingWitness::Cancel(signature),
                )),
                keypair: old_state.common.refund_keypair,
                amount: old_state.common.contract.amount,
                fee: ctx.input_fee,
            });

            let txid = ctx
                .mint
                .finalize_and_submit_transaction(dbtx, old_state.common.operation_id, tx_builder)
                .await
                .expect("Cannot claim input, additional funding needed");

            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    SendRefundEvent { txid },
                )
                .await;
        }
    }
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
        .ln_await_preimage(outpoint, contract.expiration)
        .await?;

    if contract.verify_preimage(&preimage) {
        return Some(preimage);
    }

    error!(target: LOG_CLIENT_MODULE_LN, "Federation returned invalid preimage {:?}", preimage);

    pending().await
}

async fn transition_preimage_sm(
    ctx: &LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    old_state: &SendStateMachine,
    preimage: Option<[u8; 32]>,
) {
    if let Some(preimage) = preimage {
        ctx.client_ctx
            .log_event(
                dbtx,
                old_state.common.operation_id,
                SendSuccessEvent { preimage },
            )
            .await;

        return;
    }

    let tx_builder = TransactionBuilder::from_input(Input {
        input: wire::Input::Ln(LightningInput::Outgoing(
            old_state.common.outpoint,
            OutgoingWitness::Refund,
        )),
        keypair: old_state.common.refund_keypair,
        amount: old_state.common.contract.amount,
        fee: ctx.input_fee,
    });

    let txid = ctx
        .mint
        .finalize_and_submit_transaction(dbtx, old_state.common.operation_id, tx_builder)
        .await
        .expect("Cannot claim input, additional funding needed");

    ctx.client_ctx
        .log_event(
            dbtx,
            old_state.common.operation_id,
            SendRefundEvent { txid },
        )
        .await;
}
