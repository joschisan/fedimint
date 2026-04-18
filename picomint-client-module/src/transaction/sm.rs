//! State machine for submitting transactions

use picomint_api_client::api::FederationApi;
use picomint_api_client::transaction::Transaction;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_logging::LOG_CLIENT_NET_API;
use picomint_redb::WriteTxRef;
use tokio::sync::watch;
use tracing::debug;

use crate::executor::{StateMachine, StateTransition as SmStateTransition};
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
    pub log_event_added_tx: watch::Sender<()>,
}

impl StateMachine for TxSubmissionStateMachine {
    const TABLE_NAME: &'static str = "tx-submission-sm";

    type Context = TxSubmissionSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx = ctx.clone();
        let transaction = self.transaction.clone();
        vec![SmStateTransition::new(
            await_outcome_sm(transaction, ctx.api.clone()),
            move |dbtx, outcome, old_state| {
                let ctx = ctx.clone();
                Box::pin(transition_outcome_sm(ctx, dbtx, old_state, outcome))
            },
        )]
    }
}

async fn await_outcome_sm(
    transaction: Transaction,
    api: FederationApi,
) -> Result<(), String> {
    debug!(target: LOG_CLIENT_NET_API, txid = %transaction.tx_hash(), "Submitting transaction");

    api.submit_transaction(transaction)
        .await
        .map_err(|e| e.to_string())
}

async fn transition_outcome_sm(
    ctx: TxSubmissionSmContext,
    dbtx: &WriteTxRef<'_>,
    old_state: TxSubmissionStateMachine,
    outcome: Result<(), String>,
) -> Option<TxSubmissionStateMachine> {
    let txid = old_state.transaction.tx_hash();
    match outcome {
        Ok(()) => {
            picomint_eventlog::log_event(
                dbtx,
                ctx.log_event_added_tx.clone(),
                Some(old_state.operation_id),
                TxAcceptEvent { txid },
            );
        }
        Err(error) => {
            picomint_eventlog::log_event(
                dbtx,
                ctx.log_event_added_tx.clone(),
                Some(old_state.operation_id),
                TxRejectEvent { txid, error },
            );
        }
    }

    None
}
