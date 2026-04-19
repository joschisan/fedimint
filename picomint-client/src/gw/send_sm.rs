use crate::executor::StateMachine;
use crate::transaction::builder_next::{Input, TransactionBuilder};
use picomint_core::wire;
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::ln::contracts::OutgoingContract;
use picomint_core::ln::{LightningInput, LightningInvoice, OutgoingWitness};
use picomint_core::secp256k1::Keypair;
use picomint_core::{Amount, OutPoint};
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;
use serde::{Deserialize, Serialize};

use super::FinalReceiveState;
use super::GwSmContext;
use super::events::{SendCancelEvent, SendSuccessEvent};

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
    Refunded,
    Failure,
    LightningRpcError(String),
}

impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = GwSmContext;
    type Outcome = Result<PaymentResponse, Cancelled>;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        let LightningInvoice::Bolt11(invoice) = self.invoice.clone();

        if invoice.is_expired() {
            return Err(Cancelled::InvoiceExpired);
        }

        if self.max_delay == 0 {
            return Err(Cancelled::TimeoutTooClose);
        }

        let Some(max_fee) = self.contract.amount.checked_sub(self.min_contract_amount) else {
            return Err(Cancelled::Underfunded);
        };

        match ctx
            .gateway
            .try_direct_swap(&invoice)
            .await
            .map_err(|e| Cancelled::RegistrationError(e.to_string()))?
        {
            Some((final_receive_state, target_federation)) => match final_receive_state {
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
                    .pay(invoice, self.max_delay, max_fee)
                    .await
                    .map_err(|e| Cancelled::LightningRpcError(e.to_string()))?;
                Ok(PaymentResponse {
                    preimage,
                    target_federation: None,
                })
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
            Ok(payment_response) => {
                let tx_builder = TransactionBuilder::from_input(Input {
                    input: wire::Input::Ln(LightningInput::Outgoing(
                        self.outpoint,
                        OutgoingWitness::Claim(payment_response.preimage),
                    )),
                    keypair: self.claim_keypair,
                    amount: self.contract.amount,
                    fee: ctx.input_fee,
                });

                let txid = ctx
                    .mint
                    .finalize_and_submit_transaction(dbtx, self.operation_id, tx_builder)
                    .await
                    .expect("Cannot claim input, additional funding needed");

                let event = SendSuccessEvent {
                    preimage: payment_response.preimage,
                    txid,
                };

                ctx.client_ctx
                    .log_event(dbtx, self.operation_id, event)
                    .await;
            }
            Err(_) => {
                let signature = ctx.keypair.sign_schnorr(self.contract.forfeit_message());

                ctx.client_ctx
                    .log_event(dbtx, self.operation_id, SendCancelEvent { signature })
                    .await;
            }
        }

        None
    }
}
