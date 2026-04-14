//! State machine for submitting transactions

use std::time::Duration;

use fedimint_api_client::api::DynGlobalApi;
use fedimint_core::TransactionId;
use fedimint_core::core::{Decoder, IntoDynInstance, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::DatabaseTransaction;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::transaction::{Transaction, TransactionSubmissionOutcome};
use fedimint_core::util::backoff_util::custom_backoff;
use fedimint_core::util::retry;
use fedimint_eventlog::Event;
use fedimint_logging::LOG_CLIENT_NET_API;
use tokio::sync::watch;
use tracing::debug;

use crate::executor::{StateMachine, StateTransition as SmStateTransition};
use crate::module::FinalClientIface;
use crate::sm::DynState;
use crate::{TxAcceptedEvent, TxRejectedEvent};

// TODO: how to prevent collisions? Generally reserve some range for custom IDs?
/// Reserved module instance id used for client-internal state machines
pub const TRANSACTION_SUBMISSION_MODULE_INSTANCE: ModuleInstanceId = 0xffff;

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
    pub api: DynGlobalApi,
    pub decoders: ModuleDecoderRegistry,
    pub client: FinalClientIface,
}

impl StateMachine for TxSubmissionStatesSM {
    const DB_PREFIX: u8 = 0;

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
                            ctx.decoders.clone(),
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
                                        TxRejectedEvent {
                                            txid,
                                            operation_id,
                                            error: error.clone(),
                                        },
                                    )
                                    .await;
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
                                        TxAcceptedEvent { txid, operation_id },
                                    )
                                    .await;
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

async fn log_tx_event<E: Event + Send>(
    ctx: &TxSubmissionSmContext,
    dbtx: &mut DatabaseTransaction<'_>,
    event: E,
) {
    ctx.client
        .get()
        .log_event_json(
            &mut dbtx.to_ref_nc(),
            E::MODULE,
            TRANSACTION_SUBMISSION_MODULE_INSTANCE,
            E::KIND,
            serde_json::to_value(event).expect("Serializable"),
            E::PERSISTENCE,
        )
        .await;
}

async fn tx_submission_trigger_rejected(
    transaction: Transaction,
    api: DynGlobalApi,
    decoders: ModuleDecoderRegistry,
    tx_submitted: watch::Sender<bool>,
) -> String {
    let txid = transaction.tx_hash();
    debug!(target: LOG_CLIENT_NET_API, %txid, "Submitting transaction");
    retry(
        "tx-submit-sm",
        custom_backoff(Duration::from_secs(2), Duration::from_mins(10), None),
        || async {
            if let TransactionSubmissionOutcome(Err(transaction_error)) = api
                .submit_transaction(transaction.clone())
                .await
                .try_into_inner(&decoders)?
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
    api: DynGlobalApi,
    mut tx_submitted: watch::Receiver<bool>,
) {
    let _ = tx_submitted.wait_for(|submitted| *submitted).await;
    api.await_transaction(txid).await;
    debug!(target: LOG_CLIENT_NET_API, %txid, "Transaction accepted in consensus");
}
