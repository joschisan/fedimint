use bitcoin::{BlockHash, Txid};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{impl_db_lookup, impl_db_record, PeerId};
use secp256k1::ecdsa::Signature;
use serde::Serialize;
use strum_macros::EnumIter;

use crate::{FederationUTXO, UnsignedTransaction, WalletOutputOutcome};

#[repr(u8)]
#[derive(Clone, EnumIter, Debug)]
pub enum DbKeyPrefix {
    BlockHash = 0x30,
    ClaimedUtxo = 0x31,
    BlockCountVote = 0x32,
    FeeRateVote = 0x33,
    FeeRateCounter = 0x34,
    UnsignedTransaction = 0x35,
    PegOutSignatures = 0x36,
    PendingTransaction = 0x37,
    PegOutBitcoinOutPoint = 0x38,
    FederationUtxo = 0x39,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct BlockHashKey(pub BlockHash);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct BlockHashKeyPrefix;

impl_db_record!(
    key = BlockHashKey,
    value = (),
    db_prefix = DbKeyPrefix::BlockHash,
);
impl_db_lookup!(key = BlockHashKey, query_prefix = BlockHashKeyPrefix);

#[derive(Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize)]
pub struct ClaimedUTXOKey(pub bitcoin::OutPoint);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct ClaimedUTXOPrefixKey;

impl_db_record!(
    key = ClaimedUTXOKey,
    value = (),
    db_prefix = DbKeyPrefix::ClaimedUtxo
);
impl_db_lookup!(key = ClaimedUTXOKey, query_prefix = ClaimedUTXOPrefixKey);

#[derive(Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize)]
pub struct FederationUTXOKey;

impl_db_record!(
    key = FederationUTXOKey,
    value = FederationUTXO,
    db_prefix = DbKeyPrefix::FederationUtxo,
);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct UnsignedTransactionKey(pub Txid);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct UnsignedTransactionPrefixKey;

impl_db_record!(
    key = UnsignedTransactionKey,
    value = UnsignedTransaction,
    db_prefix = DbKeyPrefix::UnsignedTransaction,
);
impl_db_lookup!(
    key = UnsignedTransactionKey,
    query_prefix = UnsignedTransactionPrefixKey
);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct PegOutSignaturesKey(pub Txid, pub PeerId);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct PegOutSignaturesTxidPrefix(pub Txid);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct PegOutSignaturesPrefix;

impl_db_record!(
    key = PegOutSignaturesKey,
    value = Vec<Signature>,
    db_prefix = DbKeyPrefix::PegOutSignatures,
);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct PendingTransactionKey(pub Txid);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct PendingTransactionPrefixKey;

impl_db_record!(
    key = PendingTransactionKey,
    value = bitcoin::Transaction,
    db_prefix = DbKeyPrefix::PendingTransaction,
);
impl_db_lookup!(
    key = PendingTransactionKey,
    query_prefix = PendingTransactionPrefixKey
);

impl_db_lookup!(
    key = PegOutSignaturesKey,
    query_prefix = PegOutSignaturesTxidPrefix
);
impl_db_lookup!(
    key = PegOutSignaturesKey,
    query_prefix = PegOutSignaturesPrefix
);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct PegOutBitcoinTransaction(pub fedimint_core::OutPoint);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct PegOutBitcoinTransactionPrefix;

impl_db_record!(
    key = PegOutBitcoinTransaction,
    value = WalletOutputOutcome,
    db_prefix = DbKeyPrefix::PegOutBitcoinOutPoint,
);

impl_db_lookup!(
    key = PegOutBitcoinTransaction,
    query_prefix = PegOutBitcoinTransactionPrefix
);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct BlockCountVoteKey(pub PeerId);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct BlockCountVotePrefix;

impl_db_record!(
    key = BlockCountVoteKey,
    value = u64,
    db_prefix = DbKeyPrefix::BlockCountVote
);

impl_db_lookup!(key = BlockCountVoteKey, query_prefix = BlockCountVotePrefix);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct FeeRateVoteKey(pub PeerId);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct FeeRateVotePrefix;

impl_db_record!(
    key = FeeRateVoteKey,
    value = Option<fedimint_core::Feerate>,
    db_prefix = DbKeyPrefix::FeeRateVote
);

impl_db_lookup!(key = FeeRateVoteKey, query_prefix = FeeRateVotePrefix);

#[derive(Clone, Debug, Encodable, Decodable, Serialize)]
pub struct FeeRateIndexKey;

impl_db_record!(
    key = FeeRateIndexKey,
    value = u64,
    db_prefix = DbKeyPrefix::FeeRateVote
);
