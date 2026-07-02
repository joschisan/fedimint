//! Per-federation receive trailer.
//!
//! The `ReceiveStateMachine` in `fedimint-gwv2-client` is purely federation-
//! local — it submits the incoming-contract tx, gathers decryption shares and
//! writes the terminal [`IncomingPaymentSucceeded`] / [`IncomingPaymentFailed`]
//! event. The trailer tails the federation client's event log and drives the
//! external side effect that makes the payment terminal from the outside
//! world's point of view:
//!
//! - Direct swap (the daemon DB has an outgoing-contract row for this payment
//!   hash): finalize the send on the source federation so the sender gets the
//!   preimage (or forfeit signature).
//! - External LN receive (no outgoing row): claim the upstream HTLC on the LDK
//!   node with the revealed preimage, or fail it back so the LN sender is
//!   refunded.
//!
//! This mirrors picomint's daemon-wide trailer but is scoped to one client
//! (fedimint event logs are per-client): one trailer is spawned per federation
//! when its client is first built (see [`AppState::select_client`]). The
//! cursor is persisted per federation in [`TrailerCursorKey`] and advanced
//! past each dispatched event. Dispatches are idempotent, so a crashed
//! trailer just re-runs the last events from the cursor on restart.

use bitcoin::hashes::sha256;
use fedimint_client::ClientHandleArc;
use fedimint_core::Amount;
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;
use fedimint_eventlog::PersistedLogEntry;
use fedimint_gwv2_client::events::{IncomingPaymentFailed, IncomingPaymentSucceeded};
use fedimint_lnv2_common::contracts::PaymentImage;
use fedimint_logging::LOG_GATEWAY;
use tracing::info;

use crate::db::{OutgoingContractKey, OutgoingContractRow, TrailerCursorKey};
use crate::{AppState, as_gw_event};

const CHUNK_SIZE: u64 = 100;

pub async fn run(state: AppState, federation_id: FederationId, client: ClientHandleArc) {
    let mut cursor = state
        .gateway_db
        .begin_transaction_nc()
        .await
        .get_value(&TrailerCursorKey(federation_id))
        .await;

    let mut log_event_added = client.log_event_added_rx();

    info!(target: LOG_GATEWAY, %federation_id, "Receive trailer running");

    loop {
        let entries = client.get_event_log(cursor, CHUNK_SIZE).await;

        for entry in &entries {
            dispatch(&state, entry).await;
            cursor = Some(entry.id().next());
        }

        if let Some(cursor) = cursor
            && !entries.is_empty()
        {
            let mut dbtx = state.gateway_db.begin_transaction().await;
            dbtx.insert_entry(&TrailerCursorKey(federation_id), &cursor)
                .await;
            dbtx.commit_tx().await;
        }

        // Caught up: block until the client orders a new event into the log.
        if (entries.len() as u64) < CHUNK_SIZE && log_event_added.changed().await.is_err() {
            break;
        }
    }
}

/// Dispatches one event-log entry, driving the external side effect that makes
/// a terminal receive final from the outside world's point of view.
async fn dispatch(state: &AppState, entry: &PersistedLogEntry) {
    let (payment_image, preimage) = if let Some(ev) = as_gw_event::<IncomingPaymentSucceeded>(entry)
    {
        let preimage = ev.preimage.expect("preimage is always recorded");
        (ev.payment_image, Some(preimage))
    } else if let Some(ev) = as_gw_event::<IncomingPaymentFailed>(entry) {
        (ev.payment_image, None)
    } else {
        return;
    };

    let PaymentImage::Hash(payment_hash) = payment_image else {
        return;
    };

    let row = state
        .gateway_db
        .begin_transaction_nc()
        .await
        .get_value(&OutgoingContractKey(OperationId::from_encodable(
            &payment_hash,
        )))
        .await;

    match row {
        Some(row) => dispatch_direct_swap(state, row, preimage).await,
        None => dispatch_ln_receive(state, payment_hash, preimage),
    }
}

/// This receive is the target of a direct swap: finalize the send on the
/// source federation so the sender gets the preimage (or forfeit signature).
async fn dispatch_direct_swap(
    state: &AppState,
    row: OutgoingContractRow,
    preimage: Option<[u8; 32]>,
) {
    state
        .finalize_send_for(
            row.federation_id,
            row.contract,
            row.outpoint,
            preimage,
            Amount::ZERO,
        )
        .await;
}

/// This receive was funded by an upstream Lightning HTLC: claim it with the
/// revealed preimage, or fail it back so the LN sender is refunded.
fn dispatch_ln_receive(state: &AppState, payment_hash: sha256::Hash, preimage: Option<[u8; 32]>) {
    match preimage {
        Some(preimage) => state.claim_htlc(payment_hash, preimage),
        None => state.fail_htlc(payment_hash),
    }
}
