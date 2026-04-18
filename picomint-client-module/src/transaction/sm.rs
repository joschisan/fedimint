//! State machine for submitting transactions

use picomint_api_client::api::FederationApi;
use picomint_api_client::transaction::{Transaction, TransactionSubmissionOutcome};
use picomint_core::backoff::{Retryable, networking_backoff};
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_logging::LOG_CLIENT_NET_API;
use picomint_redb::WriteTxRef;
use tokio::sync::watch;
use tracing::debug;

use crate::executor::{StateMachine, StateTransition as SmStateTransition};
use crate::{TxAcceptEvent, TxRejectEvent};

/// State machine that (re-)submits a transaction until it is either accepted
/// or rejected by the federation.
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
        let transaction = self.transaction.clone();
        vec![
            SmStateTransition::new(
                await_tx_rejected_sm(transaction.clone(), ctx.api.clone()),
                {
                    let ctx = ctx.clone();
                    move |dbtx, error, old_state| {
                        let ctx = ctx.clone();
                        Box::pin(transition_tx_rejected_sm(ctx, dbtx, old_state, error))
                    }
                },
            ),
            SmStateTransition::new(
                await_tx_accepted_sm(transaction, ctx.api.clone()),
                {
                    let ctx = ctx.clone();
                    move |dbtx, (), old_state| {
                        let ctx = ctx.clone();
                        Box::pin(transition_tx_accepted_sm(ctx, dbtx, old_state))
                    }
                },
            ),
        ]
    }
}

async fn await_tx_rejected_sm(transaction: Transaction, api: FederationApi) -> String {
    debug!(target: LOG_CLIENT_NET_API, txid = %transaction.tx_hash(), "Submitting transaction");

    (|| async {
        match api.submit_transaction(transaction.clone()).await {
            TransactionSubmissionOutcome(Err(transaction_error)) => {
                Ok(transaction_error.to_string())
            }
            TransactionSubmissionOutcome(Ok(..)) => {
                Err(anyhow::anyhow!("Transaction is still valid"))
            }
        }
    })
    .retry(networking_backoff())
    .await
    .expect("networking_backoff retries forever")
}

async fn await_tx_accepted_sm(transaction: Transaction, api: FederationApi) {
    api.await_transaction(transaction.tx_hash()).await;
}

async fn transition_tx_rejected_sm(
    ctx: TxSubmissionSmContext,
    dbtx: &WriteTxRef<'_>,
    old_state: TxSubmissionStateMachine,
    error: String,
) -> Option<TxSubmissionStateMachine> {
    picomint_eventlog::log_event(
        dbtx,
        ctx.log_event_added_tx.clone(),
        Some(old_state.operation_id),
        TxRejectEvent {
            txid: old_state.transaction.tx_hash(),
            error,
        },
    );

    None
}

async fn transition_tx_accepted_sm(
    ctx: TxSubmissionSmContext,
    dbtx: &WriteTxRef<'_>,
    old_state: TxSubmissionStateMachine,
) -> Option<TxSubmissionStateMachine> {
    picomint_eventlog::log_event(
        dbtx,
        ctx.log_event_added_tx.clone(),
        Some(old_state.operation_id),
        TxAcceptEvent {
            txid: old_state.transaction.tx_hash(),
        },
    );

    None
}
