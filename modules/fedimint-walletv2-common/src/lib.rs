#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]

use std::collections::BTreeMap;

use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::{BlockHash, Network, Txid};
use config::WalletClientConfig;
use fedimint_core::core::{Decoder, ModuleInstanceId, ModuleKind};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleConsensusVersion};
use fedimint_core::{
    extensible_associated_module_type, plugin_types_trait_impl_common, Feerate, NumPeersExt, PeerId,
};
use miniscript::Descriptor;
use secp256k1::ecdsa::Signature;
use secp256k1::{PublicKey, Scalar};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::error;

use crate::txoproof::{PegInProof, PegInProofError};

pub mod config;
pub mod endpoint_constants;
pub mod envs;
pub mod txoproof;

pub const KIND: ModuleKind = ModuleKind::from_static_str("wallet");
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(2, 1);

/// Used for estimating a feerate that will confirm within a target number of
/// blocks.
///
/// Since the wallet's UTXOs are a shared resource, we need to reduce the risk
/// of a peg-out transaction getting stuck in the mempool, hence we use a low
/// confirmation target. Other fee bumping techniques, such as RBF and CPFP, can
/// help mitigate this problem but are out-of-scope for this version of the
/// wallet.
pub const CONFIRMATION_TARGET: u16 = 1;

/// To further mitigate the risk of a peg-out transaction getting stuck in the
/// mempool, we multiply the feerate estimate returned from the backend by this
/// value.
pub const FEERATE_MULTIPLIER: u64 = 4;

pub fn descriptor(
    pks: &BTreeMap<PeerId, PublicKey>,
    tweak: &sha256::Hash,
) -> Descriptor<PublicKey> {
    let tweaked_pks = pks
        .values()
        .map(|pk| tweak_public_key(pk, tweak))
        .collect::<Vec<PublicKey>>();

    if pks.len() == 1 {
        Descriptor::new_wpkh(tweaked_pks[0])
    } else {
        Descriptor::new_wsh_sortedmulti(pks.to_num_peers().threshold(), tweaked_pks)
    }
    .expect("Failed to construct Descriptor")
}

pub fn tweak_public_key(pk: &PublicKey, tweak: &sha256::Hash) -> PublicKey {
    pk.add_exp_tweak(
        secp256k1::SECP256K1,
        &Scalar::from_be_bytes(tweak.to_byte_array()).expect("Hash is within field order"),
    )
    .expect("Failed to tweak bitcoin public key")
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum WalletConsensusItem {
    BlockCount(u64),
    Feerate(Option<Feerate>),
    PegOutSignature(PegOutSignatures),
    #[encodable_default]
    Default {
        variant: u64,
        bytes: Vec<u8>,
    },
}

impl std::fmt::Display for WalletConsensusItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WalletConsensusItem::BlockCount(count) => {
                write!(f, "Wallet Block Count {count}")
            }
            WalletConsensusItem::Feerate(feerate) => {
                write!(f, "Wallet Feerate Vote {:?}", feerate)
            }
            WalletConsensusItem::PegOutSignature(sig) => {
                write!(f, "Wallet PegOut signatures for Bitcoin TxId {}", sig.txid)
            }
            WalletConsensusItem::Default { variant, .. } => {
                write!(f, "Unknown Wallet CI variant={variant}")
            }
        }
    }
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, Serialize, Deserialize, Encodable, Decodable)]
pub struct PegOutSignatures {
    pub txid: Txid,
    pub signatures: Vec<Signature>,
}

extensible_associated_module_type!(
    WalletOutputOutcome,
    WalletOutputOutcomeV0,
    UnknownWalletOutputOutcomeVariantError
);

impl WalletOutputOutcome {
    pub fn new_v0(txid: bitcoin::Txid) -> WalletOutputOutcome {
        WalletOutputOutcome::V0(WalletOutputOutcomeV0(txid))
    }
}

/// Contains the Bitcoin transaction id of the transaction created by the
/// withdraw request
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct WalletOutputOutcomeV0(pub bitcoin::Txid);

impl std::fmt::Display for WalletOutputOutcomeV0 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wallet PegOut Bitcoin TxId {}", self.0)
    }
}

#[derive(Debug)]
pub struct WalletCommonInit;

impl CommonModuleInit for WalletCommonInit {
    const CONSENSUS_VERSION: ModuleConsensusVersion = MODULE_CONSENSUS_VERSION;
    const KIND: ModuleKind = KIND;

    type ClientConfig = WalletClientConfig;

    fn decoder() -> Decoder {
        WalletModuleTypes::decoder()
    }
}

extensible_associated_module_type!(WalletInput, WalletInputV0, UnknownWalletInputVariantError);

impl WalletInput {
    pub fn new_v0(proof: PegInProof, tweak: PublicKey, feerate: u64) -> WalletInput {
        WalletInput::V0(WalletInputV0 {
            proof,
            tweak,
            fee_index: feerate,
        })
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct WalletInputV0 {
    pub proof: PegInProof,
    pub tweak: PublicKey,
    pub fee_index: u64,
}

impl std::fmt::Display for WalletInputV0 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Wallet PegIn with Bitcoin TxId {}",
            self.proof.outpoint().txid
        )
    }
}

extensible_associated_module_type!(
    WalletOutput,
    WalletOutputV0,
    UnknownWalletOutputVariantError
);

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct WalletOutputV0 {
    pub address: bitcoin::Address<NetworkUnchecked>,
    #[serde(with = "bitcoin::amount::serde::as_sat")]
    pub amount: bitcoin::Amount,
    pub fee_index: u64,
}

impl std::fmt::Display for WalletOutputV0 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Wallet PegOut {} to {}",
            self.amount,
            self.address.clone().assume_checked()
        )
    }
}

pub struct WalletModuleTypes;

plugin_types_trait_impl_common!(
    KIND,
    WalletModuleTypes,
    WalletClientConfig,
    WalletInput,
    WalletOutput,
    WalletOutputOutcome,
    WalletConsensusItem,
    WalletInputError,
    WalletOutputError
);

#[derive(Debug, Error, Encodable, Decodable, Hash, Clone, Eq, PartialEq)]
pub enum WalletCreationError {
    #[error("Connected bitcoind is on wrong network, expected {0}, got {1}")]
    WrongNetwork(Network, Network),
    #[error("Error querying bitcoind: {0}")]
    RpcError(String),
}

#[derive(Debug, Error, Encodable, Decodable, Hash, Clone, Eq, PartialEq)]
pub enum WalletInputError {
    #[error("The wallet input version is not supported by this federation")]
    UnknownInputVariant(#[from] UnknownWalletInputVariantError),
    #[error("Invalid peg-in proof: {0}")]
    PegInProofError(#[from] PegInProofError),
    #[error("The peg-in was already claimed")]
    PegInAlreadyClaimed,
    #[error("Unknown block hash in peg-in proof: {0}")]
    UnknownPegInProofBlock(BlockHash),
    #[error("Wrong output script")]
    WrongOutputScript,
    #[error("The feerate index is outdated. Please construct an new transaction.")]
    IncorrectFeeRateIndex,
    #[error("No up to date feerate is available at the moment. Please try again later.")]
    NoFeerateAvailable,
    #[error("Constructing the pegin transaction caused and arithmetic overflow")]
    ArithmeticOverflow,
}

#[derive(Debug, Error, Encodable, Decodable, Hash, Clone, Eq, PartialEq)]
pub enum WalletOutputError {
    #[error("The wallet output version is not supported by this federation")]
    UnknownOutputVariant(#[from] UnknownWalletOutputVariantError),
    #[error("Connected bitcoind is on wrong network, expected {0}, got {1}")]
    WrongNetwork(Network, Network),
    #[error("Peg out amount was under the dust limit")]
    PegOutUnderDustLimit,
    #[error("The federation does not have any funds yet")]
    NoFederationUTXO,
    #[error("The feerate index is outdated. Please construct an new transaction.")]
    IncorrectFeeRateIndex,
    #[error("No up to date feerate is available at the moment. Please try again later.")]
    NoFeerateAvailable,
    #[error("Constructing the pegout transaction caused and arithmetic overflow")]
    ArithmeticOverflow,
}
