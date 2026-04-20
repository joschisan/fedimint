//! Freestanding API handlers for [`super::Wallet`].

use bitcoin::{Amount, Txid};
use picomint_core::OutPoint;
use picomint_core::module::ApiError;
use picomint_core::wallet::{FederationWallet, OutputInfo, TxInfo};

use super::Wallet;
use super::db::FEDERATION_WALLET;

pub async fn consensus_block_count(wallet: &Wallet, _: ()) -> Result<u64, ApiError> {
    let tx = wallet.db.begin_read();
    Ok(wallet.consensus_block_count(&tx))
}

pub async fn consensus_feerate(wallet: &Wallet, _: ()) -> Result<Option<u64>, ApiError> {
    let tx = wallet.db.begin_read();
    Ok(wallet.consensus_feerate(&tx))
}

pub async fn federation_wallet(
    wallet: &Wallet,
    _: (),
) -> Result<Option<FederationWallet>, ApiError> {
    Ok(wallet.db.begin_read().get(&FEDERATION_WALLET, &()))
}

pub async fn send_fee(wallet: &Wallet, _: ()) -> Result<Option<Amount>, ApiError> {
    let dbtx = wallet.db.begin_write();
    let v = wallet.send_fee(&dbtx.as_ref());
    dbtx.commit();
    Ok(v)
}

pub async fn receive_fee(wallet: &Wallet, _: ()) -> Result<Option<Amount>, ApiError> {
    let dbtx = wallet.db.begin_write();
    let v = wallet.receive_fee(&dbtx.as_ref());
    dbtx.commit();
    Ok(v)
}

pub async fn tx_id(wallet: &Wallet, outpoint: OutPoint) -> Result<Option<Txid>, ApiError> {
    let dbtx = wallet.db.begin_write();
    let v = wallet.tx_id(&dbtx.as_ref(), outpoint);
    dbtx.commit();
    Ok(v)
}

pub async fn output_info_slice(
    wallet: &Wallet,
    (start, end): (u64, u64),
) -> Result<Vec<OutputInfo>, ApiError> {
    let dbtx = wallet.db.begin_write();
    let v = wallet.get_outputs(&dbtx.as_ref(), start, end);
    dbtx.commit();
    Ok(v)
}

pub async fn pending_tx_chain(wallet: &Wallet, _: ()) -> Result<Vec<TxInfo>, ApiError> {
    let dbtx = wallet.db.begin_write();
    let v = wallet.pending_tx_chain(&dbtx.as_ref());
    dbtx.commit();
    Ok(v)
}

pub async fn tx_chain(wallet: &Wallet, _: ()) -> Result<Vec<TxInfo>, ApiError> {
    let dbtx = wallet.db.begin_write();
    let v = wallet.tx_chain(&dbtx.as_ref());
    dbtx.commit();
    Ok(v)
}
