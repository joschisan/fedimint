use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_core::TransactionId;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;

use crate::WalletClientContext;
use crate::events::{ReceivePaymentStatus, ReceivePaymentUpdateEvent};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveStateMachine {
    pub common: ReceiveSMCommon,
    pub state: ReceiveSMState,
}

picomint_redb::consensus_value!(ReceiveStateMachine);

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveSMCommon {
    pub operation_id: OperationId,
    pub txid: TransactionId,
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Funding,
}

impl StateMachine for ReceiveStateMachine {
    const TABLE_NAME: &'static str = "receive-sm";

    type Context = WalletClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx = ctx.clone();
        let operation_id = self.common.operation_id;
        let txid = self.common.txid;
        vec![SmStateTransition::new(
            await_funding_sm(ctx.clone(), operation_id, txid),
            move |dbtx, result, old_state| {
                let ctx = ctx.clone();
                Box::pin(transition_funding_sm(ctx, dbtx, result, old_state))
            },
        )]
    }
}

async fn await_funding_sm(
    ctx: WalletClientContext,
    operation_id: OperationId,
    txid: TransactionId,
) -> Result<(), String> {
    ctx.client_ctx.await_tx_accepted(operation_id, txid).await
}

async fn transition_funding_sm(
    ctx: WalletClientContext,
    dbtx: &WriteTxRef<'_>,
    result: Result<(), String>,
    old_state: ReceiveStateMachine,
) -> Option<ReceiveStateMachine> {
    let status = match result {
        Ok(()) => ReceivePaymentStatus::Success,
        Err(_) => ReceivePaymentStatus::Aborted,
    };

    ctx.client_ctx
        .log_event(
            dbtx,
            old_state.common.operation_id,
            ReceivePaymentUpdateEvent { status },
        )
        .await;

    None
}
