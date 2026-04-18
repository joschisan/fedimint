use crate::executor::StateMachine;
use crate::transaction::{ClientInput, ClientInputBundle};
use picomint_core::OutPoint;
use picomint_core::core::OperationId;
use picomint_core::ln::LightningInput;
use picomint_core::ln::contracts::IncomingContract;
use picomint_core::secp256k1::Keypair;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;
use tpe::AggregateDecryptionKey;

use super::LightningClientContext;
use super::events::{ReceiveEvent, ReceiveExpiryEvent};

/// State machine that waits on the receipt of a Lightning payment. Terminates
/// when the incoming contract is either claimed or expires.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveStateMachine {
    pub operation_id: OperationId,
    pub contract: IncomingContract,
    pub claim_keypair: Keypair,
    pub agg_decryption_key: AggregateDecryptionKey,
}

picomint_redb::consensus_value!(ReceiveStateMachine);

impl StateMachine for ReceiveStateMachine {
    const TABLE_NAME: &'static str = "receive-sm";

    type Context = LightningClientContext;
    type Outcome = Option<OutPoint>;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        ctx.client_ctx
            .module_api()
            .ln_await_incoming_contract(
                &self.contract.contract_id(),
                self.contract.commitment.expiration,
            )
            .await
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        let Some(outpoint) = outcome else {
            ctx.client_ctx
                .log_event(dbtx, self.operation_id, ReceiveExpiryEvent)
                .await;
            return None;
        };

        let client_input = ClientInput::<LightningInput> {
            input: LightningInput::Incoming(outpoint, self.agg_decryption_key),
            amount: self.contract.commitment.amount,
            keys: vec![self.claim_keypair],
        };

        let txid = ctx
            .client_ctx
            .claim_inputs(
                dbtx,
                ClientInputBundle::new(vec![client_input]),
                self.operation_id,
            )
            .await
            .expect("Cannot claim input, additional funding needed");

        ctx.client_ctx
            .log_event(
                dbtx,
                self.operation_id,
                ReceiveEvent {
                    txid,
                    amount: self.contract.commitment.amount,
                },
            )
            .await;

        None
    }
}
