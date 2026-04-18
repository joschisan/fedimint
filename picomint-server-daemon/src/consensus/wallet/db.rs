use bitcoin::{TxOut, Txid};
use picomint_core::PeerId;
use picomint_core::wallet::TxInfo;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::table;
use secp256k1::ecdsa::Signature;
use serde::Serialize;

use super::{FederationTx, FederationWallet};

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct Output(pub bitcoin::OutPoint, pub TxOut);

picomint_redb::consensus_value!(Output);

/// Newtype wrapper for `bitcoin::Txid` — lets us impl `redb::Key` locally
/// (orphan rules forbid impling it on the foreign `Txid`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Encodable, Decodable)]
pub struct TxidKey(pub Txid);

picomint_redb::consensus_key!(TxidKey);

/// Vec of ecdsa signatures — wrapped so we can impl `redb::Value` locally.
#[derive(Clone, Debug, Encodable, Decodable)]
pub struct Signatures(pub Vec<Signature>);

picomint_redb::consensus_value!(Signatures);

table!(
    OUTPUT,
    u64 => Output,
    "output",
);

table!(
    SPENT_OUTPUT,
    u64 => (),
    "spent-output",
);

table!(
    FEDERATION_WALLET,
    () => FederationWallet,
    "federation-wallet",
);

table!(
    TX_INFO,
    u64 => TxInfo,
    "tx-info",
);

table!(
    TX_INFO_INDEX,
    picomint_core::OutPoint => u64,
    "tx-info-index",
);

table!(
    UNSIGNED_TX,
    TxidKey => FederationTx,
    "unsigned-tx",
);

table!(
    SIGNATURES,
    (TxidKey, PeerId) => Signatures,
    "signatures",
);

table!(
    UNCONFIRMED_TX,
    TxidKey => FederationTx,
    "unconfirmed-tx",
);

table!(
    BLOCK_COUNT_VOTE,
    PeerId => u64,
    "block-count-vote",
);

table!(
    FEE_RATE_VOTE,
    PeerId => Option<u64>,
    "fee-rate-vote",
);
