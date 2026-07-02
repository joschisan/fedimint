//! On-disk SQLite mirror of the gwv2 payment events.
//!
//! One trailer task per federation reads that federation client's event log
//! and `INSERT`s rows into `{DATA_DIR}/analytics/analytics.sqlite`. One table
//! per event kind plus `outgoing_payments` and `incoming_payments` views that
//! each stitch the relevant event tables into a single row per payment.
//! Fedimint event logs are per-client and their entries carry no federation
//! or operation id, so each trailer tags its rows with its federation id and
//! the views correlate rows by the payment image every gwv2 event carries.
//!
//! The file is **wiped on every gateway startup** — analytics state is
//! derived, not authoritative. The event logs in the client databases are the
//! source of truth; each trailer replays from position 0 when its federation's
//! client is (lazily) first built after boot.
//!
//! Users and agents inspect the db directly via `sqlite3 analytics.sqlite`.
//! No query transport is layered on top.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use fedimint_client::ClientHandleArc;
use fedimint_core::config::FederationId;
use fedimint_core::encoding::Encodable as _;
use fedimint_eventlog::PersistedLogEntry;
use fedimint_gwv2_client::events::{
    IncomingPaymentFailed, IncomingPaymentStarted, IncomingPaymentSucceeded, OutgoingPaymentFailed,
    OutgoingPaymentStarted, OutgoingPaymentSucceeded,
};
use fedimint_logging::LOG_GATEWAY;
use rusqlite::Connection;
use tokio::sync::Mutex;

use crate::as_gw_event;

const CHUNK_SIZE: u64 = 10_000;

/// Sub-directory inside `DATA_DIR` that holds the SQLite analytics DB and
/// its WAL/SHM sidecar files. The whole directory is wiped on every
/// startup so we don't have to special-case individual files.
pub const ANALYTICS_DIR: &str = "analytics";
/// Filename of the analytics DB inside `ANALYTICS_DIR`.
pub const ANALYTICS_FILE: &str = "analytics.sqlite";

/// Shared handle to the analytics SQLite connection. All trailer tasks and
/// any future readers go through this single mutex-guarded connection — fine
/// because SQLite serializes writes internally anyway and our write volume
/// is bounded by event-log throughput.
#[derive(Clone)]
pub struct Analytics {
    conn: Arc<Mutex<Connection>>,
}

impl Analytics {
    /// Wipe `{DATA_DIR}/analytics/`, recreate it, and open a fresh SQLite
    /// DB with the schema + `outgoing_payments` / `incoming_payments`
    /// views installed. Analytics state is always rebuilt from the client
    /// event logs on startup, so we don't preserve anything across
    /// restarts.
    pub fn wipe_and_init(data_dir: &Path) -> anyhow::Result<Self> {
        let dir: PathBuf = data_dir.join(ANALYTICS_DIR);
        // A full directory wipe handles the db file and its WAL/SHM sidecars
        // in one shot.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).context("failed to create analytics dir")?;

        let conn = Connection::open(dir.join(ANALYTICS_FILE))
            .context("failed to open analytics.sqlite")?;
        // WAL mode: readers don't block the writer, writer doesn't block
        // readers. Critical for concurrent `sqlite3` CLI access while the
        // gateway is running.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;

        conn.execute_batch(SCHEMA_SQL)
            .context("failed to install analytics schema")?;

        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }
}

/// Schema + the two per-direction payment views. `fee_msat` is the gateway's
/// realized total fee for the payment: on the outgoing side it includes the
/// LN routing budget (fedimint's LNv2 fee model doesn't split the two), on
/// the incoming side it's the receive fee retained from the invoice amount.
const SCHEMA_SQL: &str = r"
CREATE TABLE send (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,   -- msecs since unix epoch
    amount_msat   INTEGER NOT NULL,
    fee_msat      INTEGER NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE TABLE send_success (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    preimage      TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE TABLE send_cancel (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    error         TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE TABLE receive (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    amount_msat   INTEGER NOT NULL,
    fee_msat      INTEGER NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE TABLE receive_success (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    preimage      TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE TABLE receive_failure (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    error         TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE INDEX idx_send_ts            ON send(ts);
CREATE INDEX idx_send_success_ts    ON send_success(ts);
CREATE INDEX idx_receive_ts         ON receive(ts);
CREATE INDEX idx_receive_success_ts ON receive_success(ts);

CREATE VIEW outgoing_payments AS
SELECT
    s.federation,
    s.payment_image,
    s.ts AS started_at,
    COALESCE(succ.ts, canc.ts) AS completed_at,
    CASE
        WHEN succ.payment_image IS NOT NULL THEN 'success'
        WHEN canc.payment_image IS NOT NULL THEN 'cancelled'
        ELSE 'pending'
    END AS status,
    s.amount_msat,
    s.fee_msat,
    succ.preimage,
    canc.error
FROM send s
LEFT JOIN send_success succ
       ON succ.federation = s.federation AND succ.payment_image = s.payment_image
LEFT JOIN send_cancel canc
       ON canc.federation = s.federation AND canc.payment_image = s.payment_image;

CREATE VIEW incoming_payments AS
SELECT
    r.federation,
    r.payment_image,
    r.ts AS started_at,
    COALESCE(succ.ts, fail.ts) AS completed_at,
    CASE
        WHEN succ.payment_image IS NOT NULL THEN 'success'
        WHEN fail.payment_image IS NOT NULL THEN 'failure'
        ELSE 'pending'
    END AS status,
    r.amount_msat,
    r.fee_msat,
    succ.preimage,
    fail.error
FROM receive r
LEFT JOIN receive_success succ
       ON succ.federation = r.federation AND succ.payment_image = r.payment_image
LEFT JOIN receive_failure fail
       ON fail.federation = r.federation AND fail.payment_image = r.payment_image;
";

/// Drain one federation client's event log forward in chunks and mirror each
/// gwv2 payment event into the SQLite analytics DB. Blocks on the client's
/// event-log notifier only when caught up with the head. Spawned once per
/// federation when its client is first built (see [`AppState::select_client`]).
///
/// [`AppState::select_client`]: crate::AppState::select_client
pub async fn trailer(analytics: Analytics, federation_id: FederationId, client: ClientHandleArc) {
    let mut cursor = None;
    let mut log_event_added = client.log_event_added_rx();

    loop {
        let entries = client.get_event_log(cursor, CHUNK_SIZE).await;
        let caught_up = (entries.len() as u64) < CHUNK_SIZE;

        if let Some(last) = entries.last() {
            cursor = Some(last.id().next());
            let analytics = analytics.clone();
            // rusqlite is sync — hop off the tokio runtime's thread pool for
            // the insert batch so we don't block other async work.
            if let Err(err) = tokio::task::spawn_blocking(move || {
                insert_batch(&analytics, federation_id, &entries)
            })
            .await
            .expect("spawn_blocking join")
            {
                tracing::error!(
                    target: LOG_GATEWAY,
                    err = %err,
                    "Analytics insert failed"
                );
            }
        }

        // Short chunk means we've caught up with the head; block until the
        // next commit. Full chunk means there might be more to drain — loop
        // without waiting.
        if caught_up && log_event_added.changed().await.is_err() {
            break;
        }
    }
}

fn insert_batch(
    analytics: &Analytics,
    federation_id: FederationId,
    entries: &[PersistedLogEntry],
) -> anyhow::Result<()> {
    let mut guard = analytics.conn.blocking_lock();
    let tx = guard.transaction()?;
    let federation = federation_id.to_string();
    for entry in entries {
        let ts = (entry.ts_usecs / 1000) as i64;
        if let Some(e) = as_gw_event::<OutgoingPaymentStarted>(entry) {
            tx.execute(
                "INSERT OR IGNORE INTO send \
                 (federation, payment_image, ts, amount_msat, fee_msat) \
                 VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    federation,
                    e.outgoing_contract.payment_image.consensus_encode_to_hex(),
                    ts,
                    e.invoice_amount.msats as i64,
                    e.outgoing_contract
                        .amount
                        .msats
                        .saturating_sub(e.invoice_amount.msats) as i64,
                ],
            )?;
        } else if let Some(e) = as_gw_event::<OutgoingPaymentSucceeded>(entry) {
            tx.execute(
                "INSERT OR IGNORE INTO send_success \
                 (federation, payment_image, ts, preimage) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    federation,
                    e.payment_image.consensus_encode_to_hex(),
                    ts,
                    e.preimage.map(hex::encode).unwrap_or_default(),
                ],
            )?;
        } else if let Some(e) = as_gw_event::<OutgoingPaymentFailed>(entry) {
            tx.execute(
                "INSERT OR IGNORE INTO send_cancel \
                 (federation, payment_image, ts, error) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    federation,
                    e.payment_image.consensus_encode_to_hex(),
                    ts,
                    format!("{:?}", e.error),
                ],
            )?;
        } else if let Some(e) = as_gw_event::<IncomingPaymentStarted>(entry) {
            tx.execute(
                "INSERT OR IGNORE INTO receive \
                 (federation, payment_image, ts, amount_msat, fee_msat) \
                 VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    federation,
                    e.incoming_contract_commitment
                        .payment_image
                        .consensus_encode_to_hex(),
                    ts,
                    e.invoice_amount.msats as i64,
                    e.invoice_amount
                        .msats
                        .saturating_sub(e.incoming_contract_commitment.amount.msats)
                        as i64,
                ],
            )?;
        } else if let Some(e) = as_gw_event::<IncomingPaymentSucceeded>(entry) {
            tx.execute(
                "INSERT OR IGNORE INTO receive_success \
                 (federation, payment_image, ts, preimage) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    federation,
                    e.payment_image.consensus_encode_to_hex(),
                    ts,
                    e.preimage.map(hex::encode).unwrap_or_default(),
                ],
            )?;
        } else if let Some(e) = as_gw_event::<IncomingPaymentFailed>(entry) {
            tx.execute(
                "INSERT OR IGNORE INTO receive_failure \
                 (federation, payment_image, ts, error) VALUES (?, ?, ?, ?)",
                rusqlite::params![
                    federation,
                    e.payment_image.consensus_encode_to_hex(),
                    ts,
                    e.error,
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
