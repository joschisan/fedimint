//! Freestanding API handlers for [`crate::Mint`].
//!
//! Each function matches one endpoint constant in
//! `picomint_mint_common::endpoint_constants` and is dispatched from
//! `Mint::handle_api` via `picomint_server_core::handler!`.

use bitcoin::hashes::sha256;
use picomint_core::OutPointRange;
use picomint_core::encoding::Encodable as _;
use picomint_core::module::ApiError;
use picomint_mint_common::RecoveryItem;
use picomint_redb::ReadTransaction;
use tbs::{BlindedMessage, BlindedSignatureShare};

use crate::Mint;
use crate::db::{BLINDED_SIGNATURE_SHARE, BLINDED_SIGNATURE_SHARE_RECOVERY, RECOVERY_ITEM};

pub async fn signature_shares(
    mint: &Mint,
    range: OutPointRange,
) -> Result<Vec<BlindedSignatureShare>, ApiError> {
    let tx = mint
        .db
        .wait_key_check(
            &BLINDED_SIGNATURE_SHARE,
            &range.start_out_point(),
            std::convert::identity,
        )
        .await
        .1;
    Ok(collect_signature_shares(&tx, range))
}

pub async fn signature_shares_recovery(
    mint: &Mint,
    messages: Vec<BlindedMessage>,
) -> Result<Vec<BlindedSignatureShare>, ApiError> {
    let tx = mint.db.begin_read().await;
    let mut shares = Vec::new();
    for message in messages {
        let share = tx
            .get(&BLINDED_SIGNATURE_SHARE_RECOVERY, &message)
            .ok_or_else(|| ApiError::bad_request("No blinded signature share found".to_string()))?;
        shares.push(share);
    }
    Ok(shares)
}

pub async fn recovery_slice(
    mint: &Mint,
    range: (u64, u64),
) -> Result<Vec<RecoveryItem>, ApiError> {
    let tx = mint.db.begin_read().await;
    Ok(collect_recovery_slice(&tx, range))
}

pub async fn recovery_slice_hash(
    mint: &Mint,
    range: (u64, u64),
) -> Result<sha256::Hash, ApiError> {
    let tx = mint.db.begin_read().await;
    Ok(collect_recovery_slice(&tx, range).consensus_hash::<sha256::Hash>())
}

pub async fn recovery_count(mint: &Mint, _: ()) -> Result<u64, ApiError> {
    let tx = mint.db.begin_read().await;
    Ok(crate::get_recovery_count(&tx))
}

fn collect_signature_shares(tx: &ReadTransaction, range: OutPointRange) -> Vec<BlindedSignatureShare> {
    tx.range(
        &BLINDED_SIGNATURE_SHARE,
        range.start_out_point()..range.end_out_point(),
    )
    .into_iter()
    .map(|(_, v)| v)
    .collect()
}

fn collect_recovery_slice(tx: &ReadTransaction, range: (u64, u64)) -> Vec<RecoveryItem> {
    tx.range(&RECOVERY_ITEM, range.0..range.1)
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}
