use fedimint_client::transaction::{ClientInput, ClientInputBundle};
use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::module::OutPointRange;
use fedimint_core::TransactionId;
use fedimint_core::core::OperationId;
use fedimint_core::db::DatabaseTransaction;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_mintv2_common::MintInput;

use crate::{MintSmContext, SpendableNote};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct InputStateMachine {
    pub common: InputSMCommon,
    pub state: InputSMState,
}

#[derive(Debug, Clone, Eq, Hash, PartialEq, Decodable, Encodable)]
pub struct InputSMCommon {
    pub operation_id: OperationId,
    pub txid: TransactionId,
    pub spendable_notes: Vec<SpendableNote>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum InputSMState {
    Pending,
    Success,
    Refunding(OutPointRange),
}

impl StateMachine for InputStateMachine {
    const DB_PREFIX: u8 = crate::client_db::DbKeyPrefix::InputStateMachine as u8;

    type Context = MintSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            InputSMState::Pending => {
                let ctx = ctx.clone();
                let txid = self.common.txid;
                vec![SmStateTransition::new(
                    await_pending_sm(ctx.clone(), txid),
                    move |dbtx, result, old_state| {
                        let ctx = ctx.clone();
                        Box::pin(transition_pending_sm(ctx, dbtx, result, old_state))
                    },
                )]
            }
            InputSMState::Success | InputSMState::Refunding(..) => vec![],
        }
    }
}

async fn await_pending_sm(ctx: MintSmContext, txid: TransactionId) -> Result<(), String> {
    ctx.client_ctx.await_tx_accepted(txid).await
}

async fn transition_pending_sm(
    ctx: MintSmContext,
    dbtx: &mut DatabaseTransaction<'_>,
    result: Result<(), String>,
    old_state: InputStateMachine,
) -> InputStateMachine {
    if result.is_ok() {
        return InputStateMachine {
            common: old_state.common,
            state: InputSMState::Success,
        };
    }

    let inputs = old_state
        .common
        .spendable_notes
        .iter()
        .map(|spendable_note| ClientInput::<MintInput> {
            input: MintInput::new_v0(spendable_note.note()),
            keys: vec![spendable_note.keypair],
            amount: spendable_note.amount(),
        })
        .collect();

    let change_range = ctx
        .client_ctx
        .claim_inputs(
            dbtx,
            ClientInputBundle::new(inputs),
            old_state.common.operation_id,
        )
        .await
        .expect("Cannot claim input, additional funding needed");

    InputStateMachine {
        common: old_state.common,
        state: InputSMState::Refunding(change_range),
    }
}
