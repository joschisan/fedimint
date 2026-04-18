use std::pin::pin;

use anyhow::Context;
use async_stream::stream;
use bitcoincore_rpc::RpcApi;
use futures::StreamExt;
use picomint_client::ClientHandleArc;
use picomint_eventlog::{EventLogEntry, EventLogId};
use picomint_wallet_client::WalletClientModule;
use picomint_wallet_client::events::{
    ReceiveEvent, SendConfirmEvent, SendEvent, SendFailureEvent,
};
use tokio::task::block_in_place;
use tracing::info;

use crate::env::{TestEnv, retry};

#[derive(Debug)]
#[allow(dead_code)]
enum WalletEvent {
    Send(SendEvent),
    SendConfirm(SendConfirmEvent),
    SendFailure(SendFailureEvent),
    Receive(ReceiveEvent),
}

fn wallet_event_stream(
    client: &ClientHandleArc,
) -> impl futures::Stream<Item = (picomint_core::core::OperationId, WalletEvent)> {
    let client = client.clone();
    let notify = client.event_notify();
    let mut next_id = EventLogId::LOG_START;

    stream! {
        loop {
            let notified = notify.notified();
            let events = client.get_event_log(Some(next_id), 100).await;

            for entry in events {
                next_id = entry.id().saturating_add(1);

                if let Some((op, event)) = try_parse_wallet_event(entry.as_raw()) {
                    yield (op, event);
                }
            }

            notified.await;
        }
    }
}

fn try_parse_wallet_event(
    entry: &EventLogEntry,
) -> Option<(picomint_core::core::OperationId, WalletEvent)> {
    let op = entry.operation_id?;
    if let Some(e) = entry.to_event() {
        return Some((op, WalletEvent::Send(e)));
    }
    if let Some(e) = entry.to_event() {
        return Some((op, WalletEvent::SendConfirm(e)));
    }
    if let Some(e) = entry.to_event() {
        return Some((op, WalletEvent::SendFailure(e)));
    }
    if let Some(e) = entry.to_event() {
        return Some((op, WalletEvent::Receive(e)));
    }
    None
}

pub async fn run_tests(env: &TestEnv, client_send: &ClientHandleArc) -> anyhow::Result<()> {
    info!("wallet: pegin + on-chain send");

    let mut send_events = pin!(wallet_event_stream(client_send));

    env.pegin(client_send, bitcoin::Amount::from_sat(100_000_000))
        .await?;

    // Drain the wallet events emitted by the pegin itself.
    let Some((_, WalletEvent::Receive(_))) = send_events.next().await else {
        panic!("Expected pegin Receive event");
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

    let Some((op, WalletEvent::Send(_))) = send_events.next().await else {
        panic!("Expected Send event");
    };
    assert_eq!(op, operation_id);

    let Some((op, WalletEvent::SendConfirm(ev))) = send_events.next().await else {
        panic!("Expected SendConfirm event");
    };
    assert_eq!(op, operation_id);
    let txid = ev.txid;

    info!(%txid, "Send confirmed, waiting for tx in mempool");

    retry("send tx in mempool", || async {
        block_in_place(|| env.bitcoind.get_mempool_entry(&txid))
            .map(|_| ())
            .context("send tx not in mempool yet")
    })
    .await?;

    info!("wallet: pegin + on-chain send passed");

    info!("wallet: zero_fee_send_aborts");

    let abort_op = client_send
        .get_first_module::<WalletClientModule>()?
        .send(
            external_address.as_unchecked().clone(),
            bitcoin::Amount::from_sat(100_000),
            Some(bitcoin::Amount::ZERO),
        )
        .await?;

    let Some((op, WalletEvent::Send(_))) = send_events.next().await else {
        panic!("Expected Send event");
    };
    assert_eq!(op, abort_op);

    let Some((op, WalletEvent::SendFailure(_))) = send_events.next().await else {
        panic!("Expected SendFailure event");
    };
    assert_eq!(op, abort_op);

    info!("wallet: zero_fee_send_aborts passed");

    Ok(())
}
