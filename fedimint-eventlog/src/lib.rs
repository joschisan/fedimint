#![allow(clippy::needless_lifetimes)]

//! Client Event Log
//!
//! Single, ordered, append-only log of all important client-side events.
//! Events that carry an `operation_id` are additionally duplicated into a
//! secondary table keyed by `(operation_id, event_log_id)` so a subscriber
//! can tail events for a specific operation cheaply via a stream API.
use std::borrow::Cow;
use std::fmt;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};

use fedimint_core::core::{ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::{IReadDatabaseTransactionOpsTyped, IWriteDatabaseTransactionOpsTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::table;
use fedimint_logging::LOG_CLIENT_EVENT_LOG;
use fedimint_redb::{Database, WriteTxRef};
use futures::Stream;
use serde::{Deserialize, Serialize};
use tokio::sync::watch;
use tracing::{debug, trace};

pub trait Event: serde::Serialize + serde::de::DeserializeOwned {
    const MODULE: Option<ModuleKind>;
    const KIND: EventKind;
}

/// Counter that resets on every restart, guaranteeing that
/// [`UnordedEventLogId`]s don't conflict with each other.
static UNORDEREDED_EVENT_LOG_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// A self-allocated ID that is mostly ordered.
///
/// Picked based on time and a counter to avoid dbtx conflicts on ID
/// allocation. Mostly, but not strictly, monotonic.
#[derive(Debug, Encodable, Decodable)]
pub struct UnordedEventLogId {
    ts_usecs: u64,
    counter: u64,
}

impl UnordedEventLogId {
    fn new() -> Self {
        Self {
            ts_usecs: u64::try_from(fedimint_core::time::duration_since_epoch().as_micros())
                .unwrap_or(u64::MAX),
            counter: UNORDEREDED_EVENT_LOG_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
        }
    }
}

/// Ordered, contiguous ID space — easy for event log followers to track.
#[derive(
    Copy,
    Clone,
    Debug,
    Encodable,
    Decodable,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
)]
pub struct EventLogId(u64);

impl EventLogId {
    pub const LOG_START: EventLogId = EventLogId(0);
    pub const LOG_END: EventLogId = EventLogId(u64::MAX);

    fn next(self) -> EventLogId {
        Self(self.0 + 1)
    }

    pub fn saturating_add(self, rhs: u64) -> EventLogId {
        Self(self.0.saturating_add(rhs))
    }
}

impl From<EventLogId> for u64 {
    fn from(value: EventLogId) -> Self {
        value.0
    }
}

impl FromStr for EventLogId {
    type Err = <u64 as FromStr>::Err;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        u64::from_str(s).map(Self)
    }
}

impl fmt::Display for EventLogId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug, Clone, Encodable, Decodable, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventKind(Cow<'static, str>);

impl EventKind {
    pub const fn from_static(value: &'static str) -> Self {
        Self(Cow::Borrowed(value))
    }
}

impl<'s> From<&'s str> for EventKind {
    fn from(value: &'s str) -> Self {
        Self(Cow::Owned(value.to_owned()))
    }
}

impl From<String> for EventKind {
    fn from(value: String) -> Self {
        Self(Cow::Owned(value))
    }
}

impl fmt::Display for EventKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Encodable, Decodable, Clone)]
pub struct EventLogEntry {
    pub kind: EventKind,

    /// Module that produced the event (if any).
    ///
    /// The meaning is "which part of the code defined this event kind". Core
    /// events referring to a module should carry the module id in
    /// `operation_id` or payload, not here.
    pub module: Option<(ModuleKind, ModuleInstanceId)>,

    /// Operation this event belongs to, if any. Set by the caller of
    /// [`log_event`]; used to index the event into
    /// [`EVENT_LOG_BY_OPERATION`] for op-scoped tailing.
    pub operation_id: Option<OperationId>,

    /// Timestamp in microseconds after unix epoch.
    pub ts_usecs: u64,

    /// Event-kind specific payload, typically json-encoded.
    pub payload: Vec<u8>,
}

impl EventLogEntry {
    pub fn module_kind(&self) -> Option<&ModuleKind> {
        self.module.as_ref().map(|m| &m.0)
    }

    pub fn module_id(&self) -> Option<ModuleInstanceId> {
        self.module.as_ref().map(|m| m.1)
    }

    pub fn to_event<E: Event>(&self) -> Option<E> {
        (self.module_kind() == E::MODULE.as_ref() && self.kind == E::KIND)
            .then(|| serde_json::from_slice(&self.payload).ok())
            .flatten()
    }
}

/// An `EventLogEntry` that was already persisted (so has an id).
#[derive(Debug, Clone)]
pub struct PersistedLogEntry {
    id: EventLogId,
    inner: EventLogEntry,
}

impl Serialize for PersistedLogEntry {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        let mut state = serializer.serialize_struct("PersistedLogEntry", 6)?;
        state.serialize_field("id", &self.id)?;
        state.serialize_field("kind", &self.inner.kind)?;
        state.serialize_field("module", &self.inner.module)?;
        state.serialize_field("operation_id", &self.inner.operation_id)?;
        state.serialize_field("ts_usecs", &self.inner.ts_usecs)?;

        let payload_value: serde_json::Value = serde_json::from_slice(&self.inner.payload)
            .unwrap_or_else(|_| serde_json::Value::String(hex::encode(&self.inner.payload)));
        state.serialize_field("payload", &payload_value)?;

        state.end()
    }
}

impl PersistedLogEntry {
    pub fn id(&self) -> EventLogId {
        self.id
    }

    pub fn as_raw(&self) -> &EventLogEntry {
        &self.inner
    }
}

impl std::ops::Deref for PersistedLogEntry {
    type Target = EventLogEntry;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

table!(
    UNORDERED_EVENT_LOG,
    UnordedEventLogId => EventLogEntry,
    "unordered-event-log",
);

table!(
    EVENT_LOG,
    EventLogId => EventLogEntry,
    "event-log",
);

table!(
    EVENT_LOG_BY_OPERATION,
    (OperationId, EventLogId) => EventLogEntry,
    "event-log-by-operation",
);

/// Read-only event log operations.
pub trait DBTransactionEventLogReadExt {
    fn get_next_event_log_id(&self) -> EventLogId;
    fn get_event_log(&self, pos: Option<EventLogId>, limit: u64) -> Vec<PersistedLogEntry>;
    fn get_events_for_operation(
        &self,
        operation_id: OperationId,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry>;
}

impl<T: IReadDatabaseTransactionOpsTyped + ?Sized> DBTransactionEventLogReadExt for T {
    fn get_next_event_log_id(&self) -> EventLogId {
        self.iter(&EVENT_LOG)
            .into_iter()
            .next_back()
            .map(|(k, _)| k.next())
            .unwrap_or_default()
    }

    fn get_event_log(&self, pos: Option<EventLogId>, limit: u64) -> Vec<PersistedLogEntry> {
        let pos = pos.unwrap_or_default();
        self.range(&EVENT_LOG, pos..pos.saturating_add(limit))
            .into_iter()
            .map(|(k, v)| PersistedLogEntry { id: k, inner: v })
            .collect()
    }

    fn get_events_for_operation(
        &self,
        operation_id: OperationId,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        let pos = pos.unwrap_or_default();
        self.range(
            &EVENT_LOG_BY_OPERATION,
            (operation_id, pos)..(operation_id, pos.saturating_add(limit)),
        )
        .into_iter()
        .map(|((_, id), entry)| PersistedLogEntry { id, inner: entry })
        .collect()
    }
}

/// Append an event to the unordered log. The ordering task picks it up after
/// commit and moves it to [`EVENT_LOG`] plus [`EVENT_LOG_BY_OPERATION`] if
/// `operation_id` is set.
pub fn log_event_raw(
    dbtx: &WriteTxRef<'_>,
    log_ordering_wakeup_tx: watch::Sender<()>,
    kind: EventKind,
    module_kind: Option<ModuleKind>,
    module_id: Option<ModuleInstanceId>,
    operation_id: Option<OperationId>,
    payload: Vec<u8>,
) {
    assert_eq!(
        module_kind.is_some(),
        module_id.is_some(),
        "Events of modules must have module_id set"
    );

    let unordered_id = UnordedEventLogId::new();
    trace!(target: LOG_CLIENT_EVENT_LOG, ?unordered_id, "New unordered event log event");

    let entry = EventLogEntry {
        kind,
        module: module_kind.map(|kind| (kind, module_id.unwrap())),
        operation_id,
        ts_usecs: unordered_id.ts_usecs,
        payload,
    };

    assert!(
        dbtx.insert(&UNORDERED_EVENT_LOG, &unordered_id, &entry)
            .is_none(),
        "Trying to overwrite event in the client event log"
    );

    dbtx.on_commit(move || {
        log_ordering_wakeup_tx.send_replace(());
    });
}

/// Typed convenience: encode an [`Event`] into the unordered log.
pub fn log_event<E: Event>(
    dbtx: &WriteTxRef<'_>,
    log_ordering_wakeup_tx: watch::Sender<()>,
    module_id: Option<ModuleInstanceId>,
    operation_id: Option<OperationId>,
    event: E,
) {
    log_event_raw(
        dbtx,
        log_ordering_wakeup_tx,
        E::KIND,
        E::MODULE,
        module_id,
        operation_id,
        serde_json::to_vec(&event).expect("Serialization can't fail"),
    );
}

/// Handles new unordered events and rewrites them fully ordered into
/// [`EVENT_LOG`] (and indexed into [`EVENT_LOG_BY_OPERATION`] when the event
/// has an `operation_id`).
pub async fn run_event_log_ordering_task(
    db: Database,
    mut log_ordering_task_wakeup: watch::Receiver<()>,
    log_event_added: watch::Sender<()>,
) {
    debug!(target: LOG_CLIENT_EVENT_LOG, "Event log ordering task started");

    let mut next_entry_id = db.begin_read().await.as_ref().get_next_event_log_id();

    loop {
        let dbtx = db.begin_write().await;
        let tx = dbtx.as_ref();

        let unordered_events = tx.iter(&UNORDERED_EVENT_LOG);
        trace!(target: LOG_CLIENT_EVENT_LOG, num=unordered_events.len(), "Fetched unordered events");

        for (unordered_id, entry) in &unordered_events {
            assert!(
                tx.remove(&UNORDERED_EVENT_LOG, unordered_id).is_some(),
                "Must never fail to remove entry"
            );

            assert!(
                tx.insert(&EVENT_LOG, &next_entry_id, entry).is_none(),
                "Must never overwrite existing event"
            );

            if let Some(operation_id) = entry.operation_id {
                assert!(
                    tx.insert(
                        &EVENT_LOG_BY_OPERATION,
                        &(operation_id, next_entry_id),
                        entry,
                    )
                    .is_none(),
                    "Must never overwrite existing event"
                );
            }

            trace!(target: LOG_CLIENT_EVENT_LOG, ?unordered_id, id=?next_entry_id, "Ordered event log event");
            next_entry_id = next_entry_id.next();
        }

        dbtx.commit().await;
        if !unordered_events.is_empty() {
            log_event_added.send_replace(());
        }

        trace!(target: LOG_CLIENT_EVENT_LOG, "Event log ordering task waits for more events");
        if log_ordering_task_wakeup.changed().await.is_err() {
            break;
        }
    }

    debug!(target: LOG_CLIENT_EVENT_LOG, "Event log ordering task finished");
}

/// Stream every event belonging to `operation_id`, in insertion order.
///
/// Yields existing events first, then live ones. The cursor is kept internally
/// — callers never manage an `EventLogId`. The stream ends when the
/// `log_event_added` watch channel is dropped (typically at client shutdown).
pub fn subscribe_operation_events(
    db: Database,
    mut log_event_added: watch::Receiver<()>,
    operation_id: OperationId,
) -> impl Stream<Item = PersistedLogEntry> {
    async_stream::stream! {
        let mut next_id = EventLogId::LOG_START;
        loop {
            let batch = db
                .begin_read()
                .await
                .as_ref()
                .get_events_for_operation(operation_id, Some(next_id), u64::MAX);
            for entry in batch {
                next_id = entry.id().next();
                yield entry;
            }
            if log_event_added.changed().await.is_err() {
                break;
            }
        }
    }
}

/// Typed variant of [`subscribe_operation_events`] — filters by
/// `E::KIND`/`E::MODULE` and decodes each matching entry.
pub fn subscribe_operation_events_typed<E: Event + 'static>(
    db: Database,
    log_event_added: watch::Receiver<()>,
    operation_id: OperationId,
) -> impl Stream<Item = E> {
    use futures::StreamExt as _;
    subscribe_operation_events(db, log_event_added, operation_id)
        .filter_map(|entry| async move { entry.to_event::<E>() })
}

#[cfg(test)]
mod tests;
