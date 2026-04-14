use std::pin::pin;

use anyhow::Context;
use async_stream::stream;
use bitcoincore_rpc::RpcApi;
use fedimint_client::ClientHandleArc;
use fedimint_eventlog::{Event, EventLogEntry, EventLogId};
use fedimint_walletv2_client::WalletClientModule;
use fedimint_walletv2_client::events::{
    ReceivePaymentEvent, ReceivePaymentUpdateEvent, SendPaymentEvent, SendPaymentStatus,
    SendPaymentUpdateEvent,
};
use futures::StreamExt;
use tokio::task::block_in_place;
use tracing::info;

use crate::env::{TestEnv, retry};

#[derive(Debug)]
#[allow(dead_code)]
enum WalletEvent {
    Send(SendPaymentEvent),
    SendUpdate(SendPaymentUpdateEvent),
    Receive(ReceivePaymentEvent),
    ReceiveUpdate(ReceivePaymentUpdateEvent),
}

fn wallet_event_stream(client: &ClientHandleArc) -> impl futures::Stream<Item = WalletEvent> {
    let client = client.clone();
    let mut log_rx = client.log_event_added_rx();
    let mut next_id = EventLogId::LOG_START;

    stream! {
        loop {
            let events = client.get_event_log(Some(next_id), 100).await;

            for entry in events {
                next_id = entry.id().saturating_add(1);

                if let Some(event) = try_parse_wallet_event(entry.as_raw()) {
                    yield event;
                }
            }

            let _ = log_rx.changed().await;
        }
    }
}

fn try_parse_wallet_event(entry: &EventLogEntry) -> Option<WalletEvent> {
    if entry.module_kind() != Some(&fedimint_walletv2_common::KIND) {
        return None;
    }

    if entry.kind == SendPaymentEvent::KIND {
        return entry.to_event().map(WalletEvent::Send);
    }

    if entry.kind == SendPaymentUpdateEvent::KIND {
        return entry.to_event().map(WalletEvent::SendUpdate);
    }

    if entry.kind == ReceivePaymentEvent::KIND {
        return entry.to_event().map(WalletEvent::Receive);
    }

    if entry.kind == ReceivePaymentUpdateEvent::KIND {
        return entry.to_event().map(WalletEvent::ReceiveUpdate);
    }

    None
}

pub async fn run_tests(env: &TestEnv, client_send: &ClientHandleArc) -> anyhow::Result<()> {
    info!("walletv2: pegin + on-chain send");

    let mut send_events = pin!(wallet_event_stream(client_send));

    env.pegin(client_send, bitcoin::Amount::from_sat(100_000_000))
        .await?;

    // Drain the walletv2 events emitted by the pegin itself.
    let Some(WalletEvent::Receive(_)) = send_events.next().await else {
        panic!("Expected pegin Receive event");
    };
    let Some(WalletEvent::ReceiveUpdate(_)) = send_events.next().await else {
        panic!("Expected pegin ReceiveUpdate event");
    };

    let external_address = block_in_place(|| env.bitcoind.get_new_address(None, None))?
        .require_network(bitcoin::Network::Regtest)?;

    info!(address = %external_address, "Sending on-chain to external address");

    let operation_id = client_send
        .get_first_module::<WalletClientModule>()?
        .send(
            external_address.as_unchecked().clone(),
            bitcoin::Amount::from_sat(100_000),
            None,
        )
        .await?;

    let Some(WalletEvent::Send(send)) = send_events.next().await else {
        panic!("Expected Send event");
    };
    assert_eq!(send.operation_id, operation_id);

    let Some(WalletEvent::SendUpdate(update)) = send_events.next().await else {
        panic!("Expected SendUpdate event");
    };
    assert_eq!(update.operation_id, operation_id);

    let SendPaymentStatus::Success(txid) = update.status else {
        panic!("On-chain send failed: {:?}", update.status);
    };

    info!(%txid, "Send confirmed, waiting for tx in mempool");

    retry("send tx in mempool", || async {
        block_in_place(|| env.bitcoind.get_mempool_entry(&txid))
            .map(|_| ())
            .context("send tx not in mempool yet")
    })
    .await?;

    info!("walletv2: pegin + on-chain send passed");

    info!("walletv2: zero_fee_send_aborts");

    let abort_op = client_send
        .get_first_module::<WalletClientModule>()?
        .send(
            external_address.as_unchecked().clone(),
            bitcoin::Amount::from_sat(100_000),
            Some(bitcoin::Amount::ZERO),
        )
        .await?;

    let Some(WalletEvent::Send(send)) = send_events.next().await else {
        panic!("Expected Send event");
    };
    assert_eq!(send.operation_id, abort_op);

    let Some(WalletEvent::SendUpdate(update)) = send_events.next().await else {
        panic!("Expected SendUpdate event");
    };
    assert_eq!(update.operation_id, abort_op);
    assert_eq!(update.status, SendPaymentStatus::Aborted);

    info!("walletv2: zero_fee_send_aborts passed");

    Ok(())
}
