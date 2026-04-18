use crate::executor::StateMachine;
use picomint_core::OutPoint;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;

use super::WalletClientContext;
use super::events::{SendConfirmEvent, SendFailureEvent};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendStateMachine {
    pub operation_id: OperationId,
    pub outpoint: OutPoint,
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

picomint_redb::consensus_value!(SendStateMachine);

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum AwaitFundingResult {
    Success(bitcoin::Txid),
    Aborted(String),
    Failure,
}

impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = WalletClientContext;
    type Outcome = AwaitFundingResult;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        if let Err(error) = ctx
            .client_ctx
            .await_tx_accepted(self.operation_id, self.outpoint.txid)
            .await
        {
            return AwaitFundingResult::Aborted(error);
        }

        match ctx
            .client_ctx
            .module_api()
            .wallet_tx_id(self.outpoint)
            .await
        {
            Some(txid) => AwaitFundingResult::Success(txid),
            None => AwaitFundingResult::Failure,
        }
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        match outcome {
            AwaitFundingResult::Success(txid) => {
                ctx.client_ctx
                    .log_event(dbtx, self.operation_id, SendConfirmEvent { txid })
                    .await;
            }
            AwaitFundingResult::Aborted(_) | AwaitFundingResult::Failure => {
                ctx.client_ctx
                    .log_event(dbtx, self.operation_id, SendFailureEvent)
                    .await;
            }
        }

        None
    }
}
