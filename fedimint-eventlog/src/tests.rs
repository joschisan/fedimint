use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use anyhow::bail;
use fedimint_core::task::TaskGroup;
use fedimint_redb::Database;
use futures::StreamExt as _;
use tokio::sync::watch;
use tokio::try_join;
use tracing::info;

use super::{EventKind, log_event_raw, run_event_log_ordering_task, subscribe_operation_events};

#[test_log::test(tokio::test)]
async fn sanity_subscribe_operation_events() {
    let db = Database::open_in_memory();
    let tg = TaskGroup::new();

    let (log_event_added_tx, log_event_added_rx) = watch::channel(());
    let (log_ordering_wakeup_tx, log_ordering_wakeup_rx) = watch::channel(());

    tg.spawn_cancellable(
        "event log ordering task",
        run_event_log_ordering_task(db.clone(), log_ordering_wakeup_rx, log_event_added_tx),
    );

    let operation_id = fedimint_core::core::OperationId::new_random();
    let counter = Arc::new(AtomicU8::new(0));

    let _ = try_join!(
        {
            let counter = counter.clone();
            let db = db.clone();
            let log_event_added_rx = log_event_added_rx.clone();
            async move {
                let mut stream = Box::pin(subscribe_operation_events(
                    db,
                    log_event_added_rx,
                    operation_id,
                ));
                while let Some(entry) = stream.next().await {
                    info!("{entry:?}");
                    assert_eq!(
                        entry.kind,
                        EventKind::from(format!(
                            "{}",
                            counter.load(std::sync::atomic::Ordering::Relaxed)
                        ))
                    );
                    if counter.load(std::sync::atomic::Ordering::Relaxed) == 4 {
                        bail!("Time to wrap up");
                    }
                    counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                }
                Ok(())
            }
        },
        async {
            for i in 0..=4 {
                let dbtx = db.begin_write().await;
                log_event_raw(
                    &dbtx.as_ref(),
                    log_ordering_wakeup_tx.clone(),
                    EventKind::from(format!("{i}")),
                    None,
                    None,
                    Some(operation_id),
                    vec![],
                );

                dbtx.commit().await;
            }

            Ok(())
        }
    );
}
