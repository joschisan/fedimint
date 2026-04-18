//! State machine for submitting transactions

use picomint_api_client::api::FederationApi;
use picomint_api_client::transaction::Transaction;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;

use crate::executor::StateMachine;
use crate::{TxAcceptEvent, TxRejectEvent};

/// State machine that submits a transaction and waits for the final outcome.
/// The server long-polls on `submit_transaction`, returning either `Ok(())`
/// once the tx has been accepted or `Err(..)` once it has been definitively
/// invalidated.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct TxSubmissionStateMachine {
    pub operation_id: OperationId,
    pub transaction: Transaction,
}

picomint_redb::consensus_value!(TxSubmissionStateMachine);

/// Context for running [`TxSubmissionStateMachine`] in a typed
/// [`crate::executor::ModuleExecutor`].
#[derive(Debug, Clone)]
pub struct TxSubmissionSmContext {
    pub api: FederationApi,
}

impl StateMachine for TxSubmissionStateMachine {
    const TABLE_NAME: &'static str = "tx-submission-sm";

    type Context = TxSubmissionSmContext;
    type Outcome = Result<(), String>;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        ctx.api
            .submit_transaction(self.transaction.clone())
            .await
            .map_err(|e| e.to_string())
    }

    async fn transition(
        &self,
        _ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        let txid = self.transaction.tx_hash();

        match outcome {
            Ok(()) => {
                picomint_eventlog::log_event(dbtx, Some(self.operation_id), TxAcceptEvent { txid });
            }
            Err(error) => {
                picomint_eventlog::log_event(
                    dbtx,
                    Some(self.operation_id),
                    TxRejectEvent { txid, error },
                );
            }
        }
        None
    }
}
