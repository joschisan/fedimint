//! State machine for submitting transactions

use picomint_api_client::api::FederationApi;
use picomint_api_client::transaction::{Transaction, TransactionSubmissionOutcome};
use picomint_core::TransactionId;
use picomint_core::backoff::networking_backoff;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::util::retry;
use picomint_eventlog::Event;
use picomint_logging::LOG_CLIENT_NET_API;
use picomint_redb::WriteTxRef;
use tokio::sync::watch;
use tracing::debug;

use crate::executor::{StateMachine, StateTransition as SmStateTransition};
use crate::{TxAcceptedEvent, TxRejectedEvent};

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine to (re-)submit a transaction until it is either accepted or
/// rejected by the federation
///
/// ```mermaid
/// flowchart LR
///     Created -- tx is accepted by consensus --> Accepted
///     Created -- tx is rejected on submission --> Rejected
/// ```
// NOTE: This struct needs to retain the same encoding as [`crate::sm::OperationState`],
// because it was used to replace it, and clients already have it persisted.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct TxSubmissionStatesSM {
    pub operation_id: OperationId,
    pub state: TxSubmissionStates,
}

picomint_redb::consensus_key!(TxSubmissionStatesSM);

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum TxSubmissionStates {
    /// The transaction has been created and potentially already been submitted,
    /// but no rejection or acceptance happened so far
    Created(Transaction),
    /// The transaction has been accepted in consensus
    ///
    /// **This state is final**
    Accepted(TransactionId),
    /// The transaction has been rejected by a quorum on submission
    ///
    /// **This state is final**
    Rejected(TransactionId, String),
    // Ideally this would be uncommented:
    // #[deprecated(since = "0.2.2", note = "all errors should be retried")]
    // but due to some rust bug/limitation it seem impossible to prevent
    // existing usages from spamming compilation output with warnings.
    NonRetryableError(String),
}

/// Context for running [`TxSubmissionStatesSM`] in a typed
/// [`crate::executor::ModuleExecutor`].
#[derive(Debug, Clone)]
pub struct TxSubmissionSmContext {
    pub api: FederationApi,
    pub log_event_added_tx: watch::Sender<()>,
}

impl StateMachine for TxSubmissionStatesSM {
    const TABLE_NAME: &'static str = "tx-submission-sm";

    type Context = TxSubmissionSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let operation_id = self.operation_id;
        let (tx_submitted_sender, tx_submitted_receiver) = watch::channel(false);
        match self.state.clone() {
            TxSubmissionStates::Created(transaction) => {
                let txid = transaction.tx_hash();
                vec![
                    SmStateTransition::new(
                        tx_submission_trigger_rejected(
                            transaction.clone(),
                            ctx.api.clone(),
                            tx_submitted_sender,
                        ),
                        {
                            let ctx = ctx.clone();
                            move |dbtx, error: String, _| {
                                let ctx = ctx.clone();
                                Box::pin(async move {
                                    log_tx_event(
                                        &ctx,
                                        dbtx,
                                        operation_id,
                                        TxRejectedEvent {
                                            txid,
                                            error: error.clone(),
                                        },
                                    );
                                    TxSubmissionStatesSM {
                                        state: TxSubmissionStates::Rejected(txid, error),
                                        operation_id,
                                    }
                                })
                            }
                        },
                    ),
                    SmStateTransition::new(
                        tx_submission_trigger_accepted(
                            txid,
                            ctx.api.clone(),
                            tx_submitted_receiver,
                        ),
                        {
                            let ctx = ctx.clone();
                            move |dbtx, (), _| {
                                let ctx = ctx.clone();
                                Box::pin(async move {
                                    log_tx_event(
                                        &ctx,
                                        dbtx,
                                        operation_id,
                                        TxAcceptedEvent { txid },
                                    );
                                    TxSubmissionStatesSM {
                                        state: TxSubmissionStates::Accepted(txid),
                                        operation_id,
                                    }
                                })
                            }
                        },
                    ),
                ]
            }
            TxSubmissionStates::Accepted(..)
            | TxSubmissionStates::Rejected(..)
            | TxSubmissionStates::NonRetryableError(..) => vec![],
        }
    }
}

fn log_tx_event<E: Event + Send>(
    ctx: &TxSubmissionSmContext,
    dbtx: &WriteTxRef<'_>,
    operation_id: OperationId,
    event: E,
) {
    picomint_eventlog::log_event(
        dbtx,
        ctx.log_event_added_tx.clone(),
        Some(operation_id),
        event,
    );
}

async fn tx_submission_trigger_rejected(
    transaction: Transaction,
    api: FederationApi,
    tx_submitted: watch::Sender<bool>,
) -> String {
    let txid = transaction.tx_hash();
    debug!(target: LOG_CLIENT_NET_API, %txid, "Submitting transaction");
    retry(
        "tx-submit-sm",
        networking_backoff(),
        || async {
            if let TransactionSubmissionOutcome(Err(transaction_error)) =
                api.submit_transaction(transaction.clone()).await
            {
                Ok(transaction_error.to_string())
            } else {
                debug!(
                    target: LOG_CLIENT_NET_API,
                    %txid,
                    "Transaction submission accepted by peer, awaiting consensus",
                );
                tx_submitted.send_replace(true);
                Err(anyhow::anyhow!("Transaction is still valid"))
            }
        },
    )
    .await
    .expect("Number of retries is has no limit")
}

async fn tx_submission_trigger_accepted(
    txid: TransactionId,
    api: FederationApi,
    mut tx_submitted: watch::Receiver<bool>,
) {
    let _ = tx_submitted.wait_for(|submitted| *submitted).await;
    api.await_transaction(txid).await;
    debug!(target: LOG_CLIENT_NET_API, %txid, "Transaction accepted in consensus");
}
