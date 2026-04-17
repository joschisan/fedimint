use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::TransactionId;
use picomint_redb::WriteTxRef;

use crate::events::{ReceivePaymentStatus, ReceivePaymentUpdateEvent};
use crate::MintSmContext;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveStateMachine {
    pub common: ReceiveSMCommon,
    pub state: ReceiveSMState,
}

picomint_core::consensus_key!(ReceiveStateMachine);

impl ReceiveStateMachine {
    pub fn update(&self, state: ReceiveSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveSMCommon {
    pub operation_id: OperationId,
    pub txid: TransactionId,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Pending,
    Success,
    Rejected(String),
}

impl StateMachine for ReceiveStateMachine {
    const TABLE_NAME: &'static str = "receive-sm";

    type Context = MintSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            ReceiveSMState::Pending => {
                let ctx_trigger = ctx.clone();
                let ctx_transition = ctx.clone();
                let operation_id = self.common.operation_id;
                let txid = self.common.txid;
                vec![SmStateTransition::new(
                    async move {
                        ctx_trigger
                            .client_ctx
                            .await_tx_accepted(operation_id, txid)
                            .await
                    },
                    move |dbtx, result, old_state| {
                        let ctx = ctx_transition.clone();
                        Box::pin(transition_tx_outcome_sm(ctx, dbtx, result, old_state))
                    },
                )]
            }
            ReceiveSMState::Success | ReceiveSMState::Rejected(..) => vec![],
        }
    }
}

async fn transition_tx_outcome_sm(
    ctx: MintSmContext,
    dbtx: &WriteTxRef<'_>,
    result: Result<(), String>,
    old_state: ReceiveStateMachine,
) -> ReceiveStateMachine {
    let (status, new_state) = match result {
        Ok(()) => (ReceivePaymentStatus::Success, ReceiveSMState::Success),
        Err(e) => (ReceivePaymentStatus::Rejected, ReceiveSMState::Rejected(e)),
    };

    ctx.client_ctx
        .log_event(
            dbtx,
            old_state.common.operation_id,
            ReceivePaymentUpdateEvent { status },
        )
        .await;

    old_state.update(new_state)
}
