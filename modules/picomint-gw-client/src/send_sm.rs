use std::fmt;

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

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendStateMachine {
    pub common: SendSMCommon,
    pub state: SendSMState,
}

picomint_core::consensus_key!(SendStateMachine);

impl SendStateMachine {
    pub fn update(&self, state: SendSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

impl fmt::Display for SendStateMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Send State Machine Operation ID: {:?} State: {}",
            self.common.operation_id, self.state
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendSMCommon {
    pub operation_id: OperationId,
    pub outpoint: OutPoint,
    pub contract: OutgoingContract,
    pub max_delay: u64,
    pub min_contract_amount: Amount,
    pub invoice: LightningInvoice,
    pub claim_keypair: Keypair,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum SendSMState {
    Sending,
    Claiming(Claiming),
    Cancelled(Cancelled),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentResponse {
    preimage: [u8; 32],
    target_federation: Option<FederationId>,
}

impl fmt::Display for SendSMState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendSMState::Sending => write!(f, "Sending"),
            SendSMState::Claiming(_) => write!(f, "Claiming"),
            SendSMState::Cancelled(_) => write!(f, "Cancelled"),
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct Claiming {
    pub preimage: [u8; 32],
    pub outpoints: Vec<OutPoint>,
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

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that handles the relay of an incoming Lightning payment.
///
/// ```mermaid
/// graph LR
/// classDef virtual fill:#fff,stroke-dasharray: 5 5
///
///     Sending -- payment is successful --> Claiming
///     Sending -- payment fails --> Cancelled
/// ```
impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = GwV2SmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            SendSMState::Sending => {
                let ctx_clone = ctx.clone();
                let max_delay = self.common.max_delay;
                let min_contract_amount = self.common.min_contract_amount;
                let invoice = self.common.invoice.clone();
                let contract = self.common.contract.clone();
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
            SendSMState::Claiming(..) | SendSMState::Cancelled(..) => vec![],
        }
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
) -> SendStateMachine {
    match result {
        Ok(payment_response) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    SendPaymentUpdateEvent {
                        status: SendPaymentStatus::Success(payment_response.preimage),
                    },
                )
                .await;

            let client_input = ClientInput::<LightningInput> {
                input: LightningInput::Outgoing(
                    old_state.common.outpoint,
                    OutgoingWitness::Claim(payment_response.preimage),
                ),
                amount: old_state.common.contract.amount,
                keys: vec![old_state.common.claim_keypair],
            };

            let outpoints = ctx
                .client_ctx
                .claim_inputs(
                    dbtx,
                    ClientInputBundle::new(vec![client_input]),
                    old_state.common.operation_id,
                )
                .await
                .expect("Cannot claim input, additional funding needed")
                .into_iter()
                .collect();

            old_state.update(SendSMState::Claiming(Claiming {
                preimage: payment_response.preimage,
                outpoints,
            }))
        }
        Err(e) => {
            let signature = ctx
                .keypair
                .sign_schnorr(old_state.common.contract.forfeit_message());

            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    SendPaymentUpdateEvent {
                        status: SendPaymentStatus::Cancelled(signature),
                    },
                )
                .await;

            old_state.update(SendSMState::Cancelled(e))
        }
    }
}
