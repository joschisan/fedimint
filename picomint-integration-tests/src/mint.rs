use std::pin::pin;
use std::sync::Arc;

use async_stream::stream;
use futures::StreamExt;
use picomint_client::mint::{ReceiveEvent, SendEvent};
use picomint_client::{Client, TxAcceptEvent, TxRejectEvent};
use picomint_core::Amount;
use picomint_core::core::OperationId;
use picomint_eventlog::{EventLogEntry, EventLogId};
use tracing::info;

use crate::env::TestEnv;

#[derive(Debug)]
#[allow(dead_code)]
enum MintEvent {
    Send(SendEvent),
    Receive(ReceiveEvent),
}

fn mint_event_stream(
    client: &Arc<Client>,
) -> impl futures::Stream<Item = (OperationId, MintEvent)> {
    let client = client.clone();
    let notify = client.event_notify();
    let mut next_id = EventLogId::LOG_START;

    stream! {
        loop {
            let notified = notify.notified();
            let events = client.get_event_log(Some(next_id), 100).await;

            for entry in events {
                next_id = entry.id().saturating_add(1);

                if let Some((op, event)) = try_parse_mint_event(entry.as_raw()) {
                    yield (op, event);
                }
            }

            notified.await;
        }
    }
}

fn try_parse_mint_event(entry: &EventLogEntry) -> Option<(OperationId, MintEvent)> {
    let op = entry.operation_id?;
    if let Some(e) = entry.to_event() {
        return Some((op, MintEvent::Send(e)));
    }
    if let Some(e) = entry.to_event() {
        return Some((op, MintEvent::Receive(e)));
    }
    None
}

/// Await the tx outcome (TxAccept or TxReject) for a specific operation_id.
async fn await_tx_outcome(client: &Arc<Client>, operation_id: OperationId) -> Result<(), String> {
    let mut stream = client.subscribe_operation_events(operation_id);
    while let Some(entry) = stream.next().await {
        if entry.to_event::<TxAcceptEvent>().is_some() {
            return Ok(());
        }
        if let Some(ev) = entry.to_event::<TxRejectEvent>() {
            return Err(ev.error);
        }
    }
    unreachable!("stream only ends at client shutdown")
}

pub async fn run_tests(env: &TestEnv, client_send: &Arc<Client>) -> anyhow::Result<()> {
    info!("mint: send_and_receive (10 iterations) + double_spend_is_rejected");

    let client_receive = env.new_client().await?;

    let mut send_events = pin!(mint_event_stream(client_send));
    let mut receive_events = pin!(mint_event_stream(&client_receive));

    for i in 0..10 {
        info!("Sending ecash payment {} of 10", i + 1);

        let ecash = client_send.mint().send(Amount::from_sats(1_000)).await?;

        let Some((_, MintEvent::Send(_))) = send_events.next().await else {
            panic!("Expected Send event");
        };

        let operation_id = client_receive.mint().receive(ecash).await?;

        let Some((op, MintEvent::Receive(_))) = receive_events.next().await else {
            panic!("Expected Receive event");
        };
        assert_eq!(op, operation_id);

        await_tx_outcome(&client_receive, operation_id)
            .await
            .expect("receive tx should be accepted");
    }

    info!("mint: send_and_receive passed");

    info!("mint: double_spend_is_rejected");

    let ecash = client_send.mint().send(Amount::from_sats(1_000)).await?;

    let Some((_, MintEvent::Send(_))) = send_events.next().await else {
        panic!("Expected Send event");
    };

    // First receive succeeds (sender receives own ecash back)
    let operation_id = client_send.mint().receive(ecash.clone()).await?;

    let Some((op, MintEvent::Receive(_))) = send_events.next().await else {
        panic!("Expected Receive event");
    };
    assert_eq!(op, operation_id);

    await_tx_outcome(client_send, operation_id)
        .await
        .expect("first receive should be accepted");

    // Second receive with same ecash is rejected
    let operation_id = client_receive.mint().receive(ecash).await?;

    let Some((op, MintEvent::Receive(_))) = receive_events.next().await else {
        panic!("Expected Receive event");
    };
    assert_eq!(op, operation_id);

    assert!(
        await_tx_outcome(&client_receive, operation_id)
            .await
            .is_err(),
        "double-spend receive should be rejected",
    );

    info!("mint: double_spend_is_rejected passed");

    client_receive.shutdown().await;

    Ok(())
}
