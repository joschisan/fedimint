//! Freestanding API handlers for [`super::Wallet`].

use bitcoin::{Amount, Txid};
use picomint_core::OutPoint;
use picomint_core::module::ApiError;
use picomint_core::wallet::{FederationWallet, OutputInfo, TxInfo};

use super::Wallet;
use super::db::FEDERATION_WALLET;

pub fn consensus_block_count(wallet: &Wallet, (): ()) -> Result<u64, ApiError> {
    let tx = wallet.db.begin_read();
    Ok(wallet.consensus_block_count(&tx))
}

pub fn consensus_feerate(wallet: &Wallet, (): ()) -> Result<Option<u64>, ApiError> {
    let tx = wallet.db.begin_read();
    Ok(wallet.consensus_feerate(&tx))
}

pub fn federation_wallet(
    wallet: &Wallet,
    (): (),
) -> Result<Option<FederationWallet>, ApiError> {
    Ok(wallet.db.begin_read().get(&FEDERATION_WALLET, &()))
}

pub fn send_fee(wallet: &Wallet, (): ()) -> Result<Option<Amount>, ApiError> {
    Ok(wallet.send_fee(&wallet.db.begin_read()))
}

pub fn receive_fee(wallet: &Wallet, (): ()) -> Result<Option<Amount>, ApiError> {
    Ok(wallet.receive_fee(&wallet.db.begin_read()))
}

pub fn tx_id(wallet: &Wallet, outpoint: OutPoint) -> Result<Option<Txid>, ApiError> {
    Ok(Wallet::tx_id(&wallet.db.begin_read(), outpoint))
}

pub fn output_info_slice(
    wallet: &Wallet,
    (start, end): (u64, u64),
) -> Result<Vec<OutputInfo>, ApiError> {
    Ok(Wallet::get_outputs(&wallet.db.begin_read(), start, end))
}

pub fn pending_tx_chain(wallet: &Wallet, (): ()) -> Result<Vec<TxInfo>, ApiError> {
    Ok(Wallet::pending_tx_chain(&wallet.db.begin_read()))
}

pub fn tx_chain(wallet: &Wallet, (): ()) -> Result<Vec<TxInfo>, ApiError> {
    Ok(Wallet::tx_chain(&wallet.db.begin_read()))
}
