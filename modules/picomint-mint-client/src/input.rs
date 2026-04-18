use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_client_module::transaction::{ClientInput, ClientInputBundle};
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::TransactionId;
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

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx = ctx.clone();
        let operation_id = self.operation_id;
        let txid = self.txid;
        vec![SmStateTransition::new(
            await_pending_sm(ctx.clone(), operation_id, txid),
            move |dbtx, result, old_state| {
                let ctx = ctx.clone();
                Box::pin(transition_pending_sm(ctx, dbtx, result, old_state))
            },
        )]
    }
}

async fn await_pending_sm(
    ctx: MintSmContext,
    operation_id: OperationId,
    txid: TransactionId,
) -> Result<(), String> {
    ctx.client_ctx.await_tx_accepted(operation_id, txid).await
}

async fn transition_pending_sm(
    ctx: MintSmContext,
    dbtx: &WriteTxRef<'_>,
    result: Result<(), String>,
    old_state: InputStateMachine,
) -> Option<InputStateMachine> {
    if result.is_ok() {
        return None;
    }

    let inputs = old_state
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
        .claim_inputs(dbtx, ClientInputBundle::new(inputs), old_state.operation_id)
        .await
        .expect("Cannot claim input, additional funding needed");

    None
}
