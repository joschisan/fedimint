//! Freestanding API handlers for [`super::Mint`].
//!
//! Each function matches one endpoint constant in
//! `picomint_core::mint::endpoint_constants` and is dispatched from
//! `Mint::handle_api` via the `handler!` macro.

use bitcoin::hashes::sha256;
use picomint_core::mint::RecoveryItem;
use picomint_core::module::ApiError;
use picomint_core::{OutPoint, TransactionId};
use picomint_encoding::Encodable as _;
use picomint_redb::ReadTransaction;
use tbs::{BlindedMessage, BlindedSignatureShare};

use super::Mint;
use super::db::{BLINDED_SIGNATURE_SHARE, BLINDED_SIGNATURE_SHARE_RECOVERY, RECOVERY_ITEM};

pub async fn signature_shares(
    mint: &Mint,
    txid: TransactionId,
) -> Result<Vec<BlindedSignatureShare>, ApiError> {
    // Wait until any BLINDED_SIGNATURE_SHARE for this txid exists. All mint
    // outputs of a given tx are signed atomically in the same consensus
    // commit, so observing one implies all are present.
    let (signatures, _tx) = mint
        .db
        .wait_table_check(&BLINDED_SIGNATURE_SHARE, |tx| {
            Some(collect_signature_shares(tx, txid)).filter(|s| !s.is_empty())
        })
        .await;

    Ok(signatures)
}

pub async fn signature_shares_recovery(
    mint: &Mint,
    messages: Vec<BlindedMessage>,
) -> Result<Vec<BlindedSignatureShare>, ApiError> {
    let mut shares = Vec::new();

    let tx = mint.db.begin_read();

    for message in messages {
        let share = tx
            .get(&BLINDED_SIGNATURE_SHARE_RECOVERY, &message)
            .ok_or_else(|| ApiError::bad_request("No blinded signature share found".to_string()))?;

        shares.push(share);
    }

    Ok(shares)
}

pub async fn recovery_slice(mint: &Mint, range: (u64, u64)) -> Result<Vec<RecoveryItem>, ApiError> {
    let tx = mint.db.begin_read();
    Ok(collect_recovery_slice(&tx, range))
}

pub async fn recovery_slice_hash(mint: &Mint, range: (u64, u64)) -> Result<sha256::Hash, ApiError> {
    let tx = mint.db.begin_read();
    Ok(collect_recovery_slice(&tx, range).consensus_hash::<sha256::Hash>())
}

pub async fn recovery_count(mint: &Mint, (): ()) -> Result<u64, ApiError> {
    let tx = mint.db.begin_read();
    Ok(super::get_recovery_count(&tx))
}

fn collect_signature_shares(
    tx: &ReadTransaction,
    txid: TransactionId,
) -> Vec<BlindedSignatureShare> {
    let bounds = OutPoint { txid, out_idx: 0 }..OutPoint {
        txid,
        out_idx: u64::MAX,
    };

    tx.range(&BLINDED_SIGNATURE_SHARE, bounds, |r| {
        r.map(|(_, v)| v).collect()
    })
}

fn collect_recovery_slice(tx: &ReadTransaction, range: (u64, u64)) -> Vec<RecoveryItem> {
    tx.range(&RECOVERY_ITEM, range.0..range.1, |r| {
        r.map(|(_, v)| v).collect()
    })
}
