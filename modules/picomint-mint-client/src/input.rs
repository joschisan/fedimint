use picomint_client_module::executor::StateMachine;
use picomint_client_module::transaction::{ClientInput, ClientInputBundle};
use picomint_core::TransactionId;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_mint_common::MintInput;
use picomint_redb::WriteTxRef;

use crate::{MintSmContext, SpendableNote};

#[derive(Debug, Clone, Eq, Hash, PartialEq, Decodable, Encodable)]
pub struct InputStateMachine {
    pub operation_id: OperationId,
    pub txid: TransactionId,
    pub spendable_notes: Vec<SpendableNote>,
}

picomint_redb::consensus_value!(InputStateMachine);

impl StateMachine for InputStateMachine {
    const TABLE_NAME: &'static str = "input-sm";

    type Context = MintSmContext;
    type Outcome = Result<(), String>;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        ctx.client_ctx
            .await_tx_accepted(self.operation_id, self.txid)
            .await
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        if outcome.is_ok() {
            return None;
        }

        let inputs = self
            .spendable_notes
            .iter()
            .map(|spendable_note| ClientInput::<MintInput> {
                input: MintInput {
                    note: spendable_note.note(),
                },
                keys: vec![spendable_note.keypair],
                amount: spendable_note.amount(),
            })
            .collect();

        ctx.client_ctx
            .claim_inputs(dbtx, ClientInputBundle::new(inputs), self.operation_id)
            .await
            .expect("Cannot claim input, additional funding needed");

        None
    }
}
