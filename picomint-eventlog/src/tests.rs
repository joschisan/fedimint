use std::sync::Arc;
use std::sync::atomic::AtomicU8;

use anyhow::bail;
use futures::StreamExt as _;
use picomint_redb::Database;
use tokio::sync::watch;
use tokio::try_join;
use tracing::info;

use super::{EventKind, log_event_raw, subscribe_operation_events};

#[test_log::test(tokio::test)]
async fn sanity_subscribe_operation_events() {
    let db = Database::open_in_memory();
    let (log_event_added_tx, log_event_added_rx) = watch::channel(());

    let operation_id = picomint_core::core::OperationId::new_random();
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
                    log_event_added_tx.clone(),
                    EventKind::from(format!("{i}")),
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
