use bitcoin::{TxOut, Txid};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{PeerId, table};
use fedimint_walletv2_common::TxInfo;
use secp256k1::ecdsa::Signature;
use serde::Serialize;

use crate::{FederationTx, FederationWallet};

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct Output(pub bitcoin::OutPoint, pub TxOut);

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
    fedimint_core::OutPoint => u64,
    "tx-info-index",
);

table!(
    UNSIGNED_TX,
    Txid => FederationTx,
    "unsigned-tx",
);

table!(
    SIGNATURES,
    (Txid, PeerId) => Vec<Signature>,
    "signatures",
);

table!(
    UNCONFIRMED_TX,
    Txid => FederationTx,
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
