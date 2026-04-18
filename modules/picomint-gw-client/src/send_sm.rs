use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_client_module::transaction::{ClientInput, ClientInputBundle};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::secp256k1::Keypair;
use picomint_core::{Amount, OutPoint};
use picomint_ln_common::contracts::OutgoingContract;
use picomint_ln_common::{LightningInput, LightningInvoice, OutgoingWitness};
use picomint_redb::WriteTxRef;
use serde::{Deserialize, Serialize};

use super::FinalReceiveState;
use super::events::{SendPaymentStatus, SendPaymentUpdateEvent};
use crate::GwV2SmContext;

/// State machine that handles the relay of an outgoing Lightning payment.
/// Terminates after the payment either succeeds (claim inputs + success event)
/// or is cancelled (cancelled event).
#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendStateMachine {
    pub operation_id: OperationId,
    pub outpoint: OutPoint,
    pub contract: OutgoingContract,
    pub max_delay: u64,
    pub min_contract_amount: Amount,
    pub invoice: LightningInvoice,
    pub claim_keypair: Keypair,
}

picomint_redb::consensus_value!(SendStateMachine);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentResponse {
    preimage: [u8; 32],
    target_federation: Option<FederationId>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable, Serialize, Deserialize)]
pub enum Cancelled {
    InvoiceExpired,
    TimeoutTooClose,
    Underfunded,
    RegistrationError(String),
    FinalizationError(String),
    Rejected,
    Refunded,
    Failure,
    LightningRpcError(String),
}

impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = GwV2SmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx_clone = ctx.clone();
        let max_delay = self.max_delay;
        let min_contract_amount = self.min_contract_amount;
        let invoice = self.invoice.clone();
        let contract = self.contract.clone();
        vec![SmStateTransition::new(
            send_payment_sm(
                ctx_clone.clone(),
                max_delay,
                min_contract_amount,
                invoice,
                contract,
            ),
            move |dbtx, result, old_state| {
                let ctx = ctx_clone.clone();
                Box::pin(transition_send_payment_sm(ctx, dbtx, old_state, result))
            },
        )]
    }
}

async fn send_payment_sm(
    ctx: GwV2SmContext,
    max_delay: u64,
    min_contract_amount: Amount,
    invoice: LightningInvoice,
    contract: OutgoingContract,
) -> Result<PaymentResponse, Cancelled> {
    let LightningInvoice::Bolt11(invoice) = invoice;

    if invoice.is_expired() {
        return Err(Cancelled::InvoiceExpired);
    }

    if max_delay == 0 {
        return Err(Cancelled::TimeoutTooClose);
    }

    let Some(max_fee) = contract.amount.checked_sub(min_contract_amount) else {
        return Err(Cancelled::Underfunded);
    };

    match ctx
        .gateway
        .try_direct_swap(&invoice)
        .await
        .map_err(|e| Cancelled::RegistrationError(e.to_string()))?
    {
        Some((final_receive_state, target_federation)) => match final_receive_state {
            FinalReceiveState::Rejected => Err(Cancelled::Rejected),
            FinalReceiveState::Success(preimage) => Ok(PaymentResponse {
                preimage,
                target_federation: Some(target_federation),
            }),
            FinalReceiveState::Refunded => Err(Cancelled::Refunded),
            FinalReceiveState::Failure => Err(Cancelled::Failure),
        },
        None => {
            let preimage = ctx
                .gateway
                .pay(invoice, max_delay, max_fee)
                .await
                .map_err(|e| Cancelled::LightningRpcError(e.to_string()))?;
            Ok(PaymentResponse {
                preimage,
                target_federation: None,
            })
        }
    }
}

async fn transition_send_payment_sm(
    ctx: GwV2SmContext,
    dbtx: &WriteTxRef<'_>,
    old_state: SendStateMachine,
    result: Result<PaymentResponse, Cancelled>,
) -> Option<SendStateMachine> {
    match result {
        Ok(payment_response) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.operation_id,
                    SendPaymentUpdateEvent {
                        status: SendPaymentStatus::Success(payment_response.preimage),
                    },
                )
                .await;

            let client_input = ClientInput::<LightningInput> {
                input: LightningInput::Outgoing(
                    old_state.outpoint,
                    OutgoingWitness::Claim(payment_response.preimage),
                ),
                amount: old_state.contract.amount,
                keys: vec![old_state.claim_keypair],
            };

            ctx.client_ctx
                .claim_inputs(
                    dbtx,
                    ClientInputBundle::new(vec![client_input]),
                    old_state.operation_id,
                )
                .await
                .expect("Cannot claim input, additional funding needed");
        }
        Err(_) => {
            let signature = ctx
                .keypair
                .sign_schnorr(old_state.contract.forfeit_message());

            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.operation_id,
                    SendPaymentUpdateEvent {
                        status: SendPaymentStatus::Cancelled(signature),
                    },
                )
                .await;
        }
    }

    None
}
