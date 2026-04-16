use std::pin::pin;

use async_stream::stream;
use futures::StreamExt;
use picomint_client::ClientHandleArc;
use picomint_core::Amount;
use picomint_eventlog::{EventLogEntry, EventLogId};
use picomint_mint_client::{
    MintClientModule, ReceivePaymentEvent, ReceivePaymentStatus, ReceivePaymentUpdateEvent,
    SendPaymentEvent,
};
use tracing::info;

use crate::env::TestEnv;

#[derive(Debug)]
#[allow(dead_code)]
enum MintEvent {
    Send(SendPaymentEvent),
    Receive(ReceivePaymentEvent),
    ReceiveUpdate(ReceivePaymentUpdateEvent),
}

fn mint_event_stream(
    client: &ClientHandleArc,
) -> impl futures::Stream<Item = (picomint_core::core::OperationId, MintEvent)> {
    let client = client.clone();
    let mut log_rx = client.log_event_added_rx();
    let mut next_id = EventLogId::LOG_START;

    stream! {
        loop {
            let events = client.get_event_log(Some(next_id), 100).await;

            for entry in events {
                next_id = entry.id().saturating_add(1);

                if let Some((op, event)) = try_parse_mint_event(entry.as_raw()) {
                    yield (op, event);
                }
            }

            let _ = log_rx.changed().await;
        }
    }
}

fn try_parse_mint_event(
    entry: &EventLogEntry,
) -> Option<(picomint_core::core::OperationId, MintEvent)> {
    let op = entry.operation_id?;
    if let Some(e) = entry.to_event() {
        return Some((op, MintEvent::Send(e)));
    }
    if let Some(e) = entry.to_event() {
        return Some((op, MintEvent::ReceiveUpdate(e)));
    }
    if let Some(e) = entry.to_event() {
        return Some((op, MintEvent::Receive(e)));
    }
    None
}

pub async fn run_tests(env: &TestEnv, client_send: &ClientHandleArc) -> anyhow::Result<()> {
    info!("mint: send_and_receive (10 iterations) + double_spend_is_rejected");

    let client_receive = env.new_client().await?;

    let mut send_events = pin!(mint_event_stream(client_send));
    let mut receive_events = pin!(mint_event_stream(&client_receive));

    for i in 0..10 {
        info!("Sending ecash payment {} of 10", i + 1);

        let ecash = client_send
            .get_first_module::<MintClientModule>()?
            .send(Amount::from_sats(1_000))
            .await?;

        let Some((_, MintEvent::Send(_))) = send_events.next().await else {
            panic!("Expected Send event");
        };

        let operation_id = client_receive
            .get_first_module::<MintClientModule>()?
            .receive(ecash)
            .await?;

        let Some((op, MintEvent::Receive(_))) = receive_events.next().await else {
            panic!("Expected Receive event");
        };
        assert_eq!(op, operation_id);

        let Some((op, MintEvent::ReceiveUpdate(update))) = receive_events.next().await else {
            panic!("Expected ReceiveUpdate event");
        };
        assert_eq!(op, operation_id);
        assert_eq!(update.status, ReceivePaymentStatus::Success);
    }

    info!("mint: send_and_receive passed");

    info!("mint: double_spend_is_rejected");

    let ecash = client_send
        .get_first_module::<MintClientModule>()?
        .send(Amount::from_sats(1_000))
        .await?;

    let Some((_, MintEvent::Send(_))) = send_events.next().await else {
        panic!("Expected Send event");
    };

    // First receive succeeds (sender receives own ecash back)
    let operation_id = client_send
        .get_first_module::<MintClientModule>()?
        .receive(ecash.clone())
        .await?;

    let Some((op, MintEvent::Receive(_))) = send_events.next().await else {
        panic!("Expected Receive event");
    };
    assert_eq!(op, operation_id);

    let Some((op, MintEvent::ReceiveUpdate(update))) = send_events.next().await else {
        panic!("Expected ReceiveUpdate event");
    };
    assert_eq!(op, operation_id);
    assert_eq!(update.status, ReceivePaymentStatus::Success);

    // Second receive with same ecash is rejected
    let operation_id = client_receive
        .get_first_module::<MintClientModule>()?
        .receive(ecash)
        .await?;

    let Some((op, MintEvent::Receive(_))) = receive_events.next().await else {
        panic!("Expected Receive event");
    };
    assert_eq!(op, operation_id);

    let Some((op, MintEvent::ReceiveUpdate(update))) = receive_events.next().await else {
        panic!("Expected ReceiveUpdate event");
    };
    assert_eq!(op, operation_id);
    assert_eq!(update.status, ReceivePaymentStatus::Rejected);

    info!("mint: double_spend_is_rejected passed");

    client_receive
        .task_group()
        .clone()
        .shutdown_join_all(None)
        .await?;

    Ok(())
}
