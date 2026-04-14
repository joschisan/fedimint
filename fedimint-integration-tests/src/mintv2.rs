use std::pin::pin;

use async_stream::stream;
use fedimint_client::ClientHandleArc;
use fedimint_core::Amount;
use fedimint_eventlog::{Event, EventLogEntry, EventLogId};
use fedimint_mintv2_client::{
    MintClientModule, ReceivePaymentEvent, ReceivePaymentStatus, ReceivePaymentUpdateEvent,
    SendPaymentEvent,
};
use futures::StreamExt;
use tracing::info;

use crate::env::TestEnv;

#[derive(Debug)]
#[allow(dead_code)]
enum MintEvent {
    Send(SendPaymentEvent),
    Receive(ReceivePaymentEvent),
    ReceiveUpdate(ReceivePaymentUpdateEvent),
}

fn mint_event_stream(client: &ClientHandleArc) -> impl futures::Stream<Item = MintEvent> {
    let client = client.clone();
    let mut log_rx = client.log_event_added_rx();
    let mut next_id = EventLogId::LOG_START;

    stream! {
        loop {
            let events = client.get_event_log(Some(next_id), 100).await;

            for entry in events {
                next_id = entry.id().saturating_add(1);

                if let Some(event) = try_parse_mint_event(entry.as_raw()) {
                    yield event;
                }
            }

            let _ = log_rx.changed().await;
        }
    }
}

fn try_parse_mint_event(entry: &EventLogEntry) -> Option<MintEvent> {
    if entry.module_kind() != Some(&fedimint_mintv2_common::KIND) {
        return None;
    }

    if entry.kind == SendPaymentEvent::KIND {
        return entry.to_event().map(MintEvent::Send);
    }

    if entry.kind == ReceivePaymentUpdateEvent::KIND {
        return entry.to_event().map(MintEvent::ReceiveUpdate);
    }

    if entry.kind == ReceivePaymentEvent::KIND {
        return entry.to_event().map(MintEvent::Receive);
    }

    None
}

pub async fn run_tests(env: &TestEnv) -> anyhow::Result<()> {
    info!("mintv2: send_and_receive (10 iterations) + double_spend_is_rejected");

    let client_send = env.new_client().await?;
    let client_receive = env.new_client().await?;

    env.pegin(&client_send, bitcoin::Amount::from_sat(100_000_000))
        .await?;

    let mut send_events = pin!(mint_event_stream(&client_send));
    let mut receive_events = pin!(mint_event_stream(&client_receive));

    for i in 0..10 {
        info!("Sending ecash payment {} of 10", i + 1);

        let ecash = client_send
            .get_first_module::<MintClientModule>()?
            .send(Amount::from_sats(1_000))
            .await?;

        let Some(MintEvent::Send(_)) = send_events.next().await else {
            panic!("Expected Send event");
        };

        let operation_id = client_receive
            .get_first_module::<MintClientModule>()?
            .receive(ecash)
            .await?;

        let Some(MintEvent::Receive(receive)) = receive_events.next().await else {
            panic!("Expected Receive event");
        };
        assert_eq!(receive.operation_id, operation_id);

        let Some(MintEvent::ReceiveUpdate(update)) = receive_events.next().await else {
            panic!("Expected ReceiveUpdate event");
        };
        assert_eq!(update.operation_id, operation_id);
        assert_eq!(update.status, ReceivePaymentStatus::Success);
    }

    info!("mintv2: send_and_receive passed");

    info!("mintv2: double_spend_is_rejected");

    let ecash = client_send
        .get_first_module::<MintClientModule>()?
        .send(Amount::from_sats(1_000))
        .await?;

    let Some(MintEvent::Send(_)) = send_events.next().await else {
        panic!("Expected Send event");
    };

    // First receive succeeds (sender receives own ecash back)
    let operation_id = client_send
        .get_first_module::<MintClientModule>()?
        .receive(ecash.clone())
        .await?;

    let Some(MintEvent::Receive(receive)) = send_events.next().await else {
        panic!("Expected Receive event");
    };
    assert_eq!(receive.operation_id, operation_id);

    let Some(MintEvent::ReceiveUpdate(update)) = send_events.next().await else {
        panic!("Expected ReceiveUpdate event");
    };
    assert_eq!(update.operation_id, operation_id);
    assert_eq!(update.status, ReceivePaymentStatus::Success);

    // Second receive with same ecash is rejected
    let operation_id = client_receive
        .get_first_module::<MintClientModule>()?
        .receive(ecash)
        .await?;

    let Some(MintEvent::Receive(receive)) = receive_events.next().await else {
        panic!("Expected Receive event");
    };
    assert_eq!(receive.operation_id, operation_id);

    let Some(MintEvent::ReceiveUpdate(update)) = receive_events.next().await else {
        panic!("Expected ReceiveUpdate event");
    };
    assert_eq!(update.operation_id, operation_id);
    assert_eq!(update.status, ReceivePaymentStatus::Rejected);

    info!("mintv2: double_spend_is_rejected passed");

    Ok(())
}
