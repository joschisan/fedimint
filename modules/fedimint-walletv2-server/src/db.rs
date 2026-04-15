use bitcoin::{TxOut, Txid};
use fedimint_core::PeerId;
use fedimint_core::db::v2::TableDef;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_walletv2_common::TxInfo;
use secp256k1::ecdsa::Signature;
use serde::Serialize;

use crate::{FederationTx, FederationWallet};

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct Output(pub bitcoin::OutPoint, pub TxOut);

pub const OUTPUT: TableDef<u64, Output> = TableDef::new("output");

pub const SPENT_OUTPUT: TableDef<u64, ()> = TableDef::new("spent_output");

pub const FEDERATION_WALLET: TableDef<(), FederationWallet> = TableDef::new("federation_wallet");

pub const TX_INFO: TableDef<u64, TxInfo> = TableDef::new("tx_info");

pub const TX_INFO_INDEX: TableDef<fedimint_core::OutPoint, u64> = TableDef::new("tx_info_index");

pub const UNSIGNED_TX: TableDef<Txid, FederationTx> = TableDef::new("unsigned_tx");

pub const SIGNATURES: TableDef<(Txid, PeerId), Vec<Signature>> = TableDef::new("signatures");

pub const UNCONFIRMED_TX: TableDef<Txid, FederationTx> = TableDef::new("unconfirmed_tx");

pub const BLOCK_COUNT_VOTE: TableDef<PeerId, u64> = TableDef::new("block_count_vote");

pub const FEE_RATE_VOTE: TableDef<PeerId, Option<u64>> = TableDef::new("fee_rate_vote");
