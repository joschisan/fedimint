use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use anyhow::bail;
use fedimint_core::db::v2::{IReadDatabaseTransactionOpsTyped as _, IWriteDatabaseTransactionOpsTyped as _};
use fedimint_core::task::TaskGroup;
use fedimint_core::{apply, async_trait_maybe_send, table};
use fedimint_redb::v2::{Database, WriteTxRef};
use tokio::sync::{broadcast, watch};
use tokio::try_join;
use tracing::info;

use super::{
    EventKind, EventLogEntry, EventLogId, EventLogTrimableId, TRIMABLE_EVENTLOG_MIN_ID_AGE,
    TRIMABLE_EVENTLOG_MIN_TS_AGE, handle_events, log_event_raw, run_event_log_ordering_task,
    trim_trimable_log,
};
use crate::{EVENT_LOG_TRIMABLE, EventLogNonTrimableTracker};

table!(
    TEST_EVENT_LOG_ID,
    () => EventLogId,
    "test-event-log-id",
);

struct TestEventLogTracker;

#[apply(async_trait_maybe_send!)]
impl EventLogNonTrimableTracker for TestEventLogTracker {
    async fn store(&mut self, dbtx: &WriteTxRef<'_>, pos: EventLogId) -> anyhow::Result<()> {
        dbtx.insert(&TEST_EVENT_LOG_ID, &(), &pos);
        Ok(())
    }

    async fn load(&mut self, dbtx: &WriteTxRef<'_>) -> anyhow::Result<Option<EventLogId>> {
        Ok(dbtx.get(&TEST_EVENT_LOG_ID, &()))
    }
}

#[test_log::test(tokio::test)]
async fn sanity_handle_events() {
    let db = Database::open_in_memory();
    let tg = TaskGroup::new();

    let (log_event_added_tx, log_event_added_rx) = watch::channel(());
    let (log_ordering_wakeup_tx, log_ordering_wakeup_rx) = watch::channel(());
    let (log_event_added_transient_tx, _log_event_added_transient_rx) = broadcast::channel(1024);

    tg.spawn_cancellable(
        "event log ordering task",
        run_event_log_ordering_task(
            db.clone(),
            log_ordering_wakeup_rx,
            log_event_added_tx,
            log_event_added_transient_tx,
        ),
    );

    let counter = Arc::new(AtomicU8::new(0));

    let _ = try_join!(
        handle_events(
            db.clone(),
            Box::new(TestEventLogTracker),
            log_event_added_rx,
            move |_dbtx, event| {
                let counter = counter.clone();
                Box::pin(async move {
                    info!("{event:?}");

                    assert_eq!(
                        event.kind,
                        EventKind::from(format!(
                            "{}",
                            counter.load(std::sync::atomic::Ordering::Relaxed)
                        ))
                    );

                    if counter.load(std::sync::atomic::Ordering::Relaxed) == 4 {
                        bail!("Time to wrap up");
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    Ok(())
                })
            },
        ),
        async {
            for i in 0..=4 {
                let dbtx = db.begin_write().await;
                log_event_raw(
                    &dbtx.as_ref(),
                    log_ordering_wakeup_tx.clone(),
                    EventKind::from(format!("{i}")),
                    None,
                    None,
                    vec![],
                    crate::EventPersistence::Persistent,
                );

                dbtx.commit().await;
            }

            Ok(())
        }
    );
}

#[test_log::test(tokio::test)]
async fn test_trim_trimable_log() {
    let db = Database::open_in_memory();

    let num_entries = (2 * TRIMABLE_EVENTLOG_MIN_ID_AGE) as usize;
    let base_timestamp = 1_000_000_000_000_000u64;
    let timestamp_increment = 60 * 1_000_000u64;

    {
        let dbtx = db.begin_write().await;
        let tx = dbtx.as_ref();

        for i in 0..num_entries {
            let id = EventLogTrimableId::from(i as u64);
            let entry = EventLogEntry {
                kind: EventKind::from(format!("test_event_{i}")),
                module: None,
                ts_usecs: base_timestamp + (i as u64 * timestamp_increment),
                payload: format!("test_payload_{i}").into_bytes(),
            };

            tx.insert(&EVENT_LOG_TRIMABLE, &id, &entry);
        }

        dbtx.commit().await;
    }

    {
        let dbtx = db.begin_read().await;
        let count = dbtx.as_ref().iter(&EVENT_LOG_TRIMABLE).len();
        assert_eq!(count, num_entries);
    }

    let entries_to_trim = TRIMABLE_EVENTLOG_MIN_ID_AGE as usize;
    let last_old_entry_timestamp =
        base_timestamp + ((entries_to_trim - 1) as u64 * timestamp_increment);
    let current_time = last_old_entry_timestamp + TRIMABLE_EVENTLOG_MIN_TS_AGE + 1;

    trim_trimable_log(&db, current_time).await;

    {
        let dbtx = db.begin_read().await;
        let entries = dbtx.as_ref().iter(&EVENT_LOG_TRIMABLE);
        let remaining_count = entries.len();

        let expected_remaining = num_entries - entries_to_trim;
        assert_eq!(remaining_count, expected_remaining);

        let remaining_ids: Vec<_> = entries.into_iter().map(|(id, _)| id.0.0).collect();

        let expected_start_id = TRIMABLE_EVENTLOG_MIN_ID_AGE;
        assert_eq!(remaining_ids[0], expected_start_id);
        assert_eq!(remaining_ids.len(), expected_remaining);
    }
}
