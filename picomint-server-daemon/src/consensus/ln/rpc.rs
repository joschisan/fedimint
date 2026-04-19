//! Freestanding API handlers for [`super::Lightning`].

use std::time::Duration;

use picomint_core::OutPoint;
use picomint_core::ln::ContractId;
use picomint_core::ln::contracts::IncomingContract;
use picomint_core::module::ApiError;
use picomint_core::util::SafeUrl;
use tokio::time::timeout;
use tpe::DecryptionKeyShare;

use super::Lightning;
use super::db::{
    DECRYPTION_KEY_SHARE, GATEWAY, INCOMING_CONTRACT_OUTPOINT, INCOMING_CONTRACT_STREAM,
    INCOMING_CONTRACT_STREAM_INDEX, OUTGOING_CONTRACT, PREIMAGE,
};

pub async fn consensus_block_count(ln: &Lightning, _: ()) -> Result<u64, ApiError> {
    let tx = ln.db.begin_read().await;
    Ok(ln.consensus_block_count(&tx))
}

pub async fn await_incoming_contract(
    ln: &Lightning,
    (contract_id, expiration): (ContractId, u64),
) -> Result<Option<OutPoint>, ApiError> {
    loop {
        // Wait for the contract to appear, or time out periodically to check
        // expiration against consensus time.
        let wait = ln.db.wait_table_check(&INCOMING_CONTRACT_OUTPOINT, |tx| {
            tx.get(&INCOMING_CONTRACT_OUTPOINT, &contract_id)
        });

        if let Ok((outpoint, _tx)) = timeout(Duration::from_secs(10), wait).await {
            return Ok(Some(outpoint));
        }

        let tx = ln.db.begin_read().await;

        if let Some(outpoint) = tx.get(&INCOMING_CONTRACT_OUTPOINT, &contract_id) {
            return Ok(Some(outpoint));
        }

        if expiration <= ln.consensus_unix_time(&tx) {
            return Ok(None);
        }
    }
}

pub async fn await_preimage(
    ln: &Lightning,
    (outpoint, expiration): (OutPoint, u64),
) -> Result<Option<[u8; 32]>, ApiError> {
    loop {
        let wait = ln
            .db
            .wait_table_check(&PREIMAGE, |tx| tx.get(&PREIMAGE, &outpoint));

        if let Ok((preimage, _tx)) = timeout(Duration::from_secs(10), wait).await {
            return Ok(Some(preimage));
        }

        let tx = ln.db.begin_read().await;

        if let Some(preimage) = tx.get(&PREIMAGE, &outpoint) {
            return Ok(Some(preimage));
        }

        if expiration <= ln.consensus_block_count(&tx) {
            return Ok(None);
        }
    }
}

pub async fn decryption_key_share(
    ln: &Lightning,
    outpoint: OutPoint,
) -> Result<DecryptionKeyShare, ApiError> {
    ln.db
        .begin_read()
        .await
        .get(&DECRYPTION_KEY_SHARE, &outpoint)
        .ok_or_else(|| ApiError::bad_request("No decryption key share found".to_string()))
}

pub async fn outgoing_contract_expiration(
    ln: &Lightning,
    outpoint: OutPoint,
) -> Result<Option<(ContractId, u64)>, ApiError> {
    let tx = ln.db.begin_read().await;

    let Some(contract) = tx.get(&OUTGOING_CONTRACT, &outpoint) else {
        return Ok(None);
    };

    let expiration = contract
        .expiration
        .saturating_sub(ln.consensus_block_count(&tx));

    Ok(Some((contract.contract_id(), expiration)))
}

pub async fn await_incoming_contracts(
    ln: &Lightning,
    (start, batch): (u64, u64),
) -> Result<(Vec<IncomingContract>, u64), ApiError> {
    if batch == 0 {
        return Err(ApiError::bad_request(
            "Batch size must be greater than 0".to_string(),
        ));
    }

    let (mut next_index, tx) = ln
        .db
        .wait_table_check(&INCOMING_CONTRACT_STREAM_INDEX, |tx| {
            tx.get(&INCOMING_CONTRACT_STREAM_INDEX, &())
                .filter(|i| *i > start)
        })
        .await;

    let contracts = tx.range(&INCOMING_CONTRACT_STREAM, start..u64::MAX, |r| {
        r.take(batch as usize).collect::<Vec<_>>()
    });

    let mut results = Vec::with_capacity(contracts.len());

    for (key, contract) in contracts {
        results.push(contract);
        next_index = key + 1;
    }

    Ok((results, next_index))
}

pub async fn gateways(ln: &Lightning, _: ()) -> Result<Vec<SafeUrl>, ApiError> {
    Ok(ln
        .db
        .begin_read()
        .await
        .iter(&GATEWAY, |r| r.map(|(url, ())| url).collect()))
}
