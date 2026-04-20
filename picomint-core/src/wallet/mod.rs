#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]

use std::collections::BTreeMap;
use std::time::Duration;

use bitcoin::hashes::{Hash, hash160, sha256};
use bitcoin::key::TapTweak;
use bitcoin::{Address, PubkeyHash, ScriptBuf, ScriptHash, Txid, WPubkeyHash, WScriptHash};
use miniscript::descriptor::Wsh;
use picomint_encoding::{Decodable, Encodable};

use crate::core::ModuleKind;
use crate::{NumPeersExt, PeerId};
use secp256k1::ecdsa::Signature;
use secp256k1::{PublicKey, Scalar, XOnlyPublicKey};
use serde::{Deserialize, Serialize};
use thiserror::Error;

pub mod config;
pub mod endpoint_constants;

pub const KIND: ModuleKind = ModuleKind::Wallet;

/// Returns a sleep duration of 1 second in test environments or 60 seconds in
/// production. Used for polling intervals where faster feedback is needed
/// during testing.
pub fn sleep_duration() -> Duration {
    if crate::envs::is_running_in_test_env() {
        Duration::from_secs(1)
    } else {
        Duration::from_mins(1)
    }
}

pub fn descriptor(pks: &BTreeMap<PeerId, PublicKey>, tweak: &sha256::Hash) -> Wsh<PublicKey> {
    Wsh::new_sortedmulti(
        pks.to_num_peers().threshold(),
        pks.values()
            .map(|pk| tweak_public_key(pk, tweak))
            .collect::<Vec<PublicKey>>(),
    )
    .expect("Failed to construct Descriptor")
}

pub fn tweak_public_key(pk: &PublicKey, tweak: &sha256::Hash) -> PublicKey {
    pk.add_exp_tweak(
        secp256k1::SECP256K1,
        &Scalar::from_be_bytes(tweak.to_byte_array()).expect("Hash is within field order"),
    )
    .expect("Failed to tweak bitcoin public key")
}

/// Returns true if the script pubkey potentially belongs to the federation.
/// This uses a probabilistic filter - only ~1/65536 of P2WSH scripts pass.
pub fn is_potential_receive(script_pubkey: &ScriptBuf, pks_hash: &sha256::Hash) -> bool {
    (pks_hash, script_pubkey)
        .consensus_hash::<sha256::Hash>()
        .to_byte_array()
        .iter()
        .take(2)
        .all(|b| *b == 0)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct FederationWallet {
    pub value: bitcoin::Amount,
    pub outpoint: bitcoin::OutPoint,
    pub tweak: sha256::Hash,
}

picomint_redb::consensus_value!(FederationWallet);

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct TxInfo {
    pub index: u64,
    pub txid: bitcoin::Txid,
    pub input: bitcoin::Amount,
    pub output: bitcoin::Amount,
    pub fee: bitcoin::Amount,
    pub vbytes: u64,
    pub created: u64,
}

picomint_redb::consensus_value!(TxInfo);

impl TxInfo {
    pub fn feerate(&self) -> u64 {
        self.fee.to_sat() / self.vbytes
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct OutputInfo {
    pub index: u64,
    pub script: ScriptBuf,
    pub value: bitcoin::Amount,
    pub spent: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub enum WalletConsensusItem {
    BlockCount(u64),
    Feerate(Option<u64>),
    Signatures(Txid, Vec<Signature>),
}

impl std::fmt::Display for WalletConsensusItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BlockCount(count) => {
                write!(f, "Wallet Block Count {count}")
            }
            Self::Feerate(feerate) => {
                write!(f, "Wallet Feerate Vote {feerate:?}")
            }
            Self::Signatures(..) => {
                write!(f, "Wallet Signatures")
            }
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct WalletInput {
    pub output_index: u64,
    pub tweak: PublicKey,
    pub fee: bitcoin::Amount,
}

impl std::fmt::Display for WalletInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wallet PegIn for output index {}", self.output_index)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct WalletOutput {
    pub destination: StandardScript,
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

impl std::fmt::Display for WalletOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Wallet PegOut {}", self.value)
    }
}

#[derive(Debug, Error, Encodable, Decodable, Hash, Clone, Eq, PartialEq)]
pub enum WalletInputError {
    #[error("The output has already been claimed")]
    OutputAlreadySpent,
    #[error("Unknown output index")]
    UnknownOutputIndex,
    #[error("The tweak does not match the output script")]
    WrongTweak,
    #[error("No up to date feerate is available at the moment. Please try again later.")]
    NoConsensusFeerateAvailable,
    #[error("The total transaction fee is too low. Please construct a new transaction.")]
    InsufficientTotalFee,
    #[error("Constructing the pegin transaction caused an arithmetic overflow")]
    ArithmeticOverflow,
}

#[derive(Debug, Error, Encodable, Decodable, Hash, Clone, Eq, PartialEq)]
pub enum WalletOutputError {
    #[error("The output value is below the dust limit.")]
    UnderDustLimit,
    #[error("The federation does not have any funds yet")]
    NoFederationUTXO,
    #[error("No up to date feerate is available at the moment. Please try again later.")]
    NoConsensusFeerateAvailable,
    #[error("The total transaction fee is too low. Please construct a new transaction.")]
    InsufficientTotalFee,
    #[error("The change value is below the dust limit.")]
    ChangeUnderDustLimit,
    #[error("Constructing the pegout transaction caused an arithmetic overflow")]
    ArithmeticOverflow,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Serialize, Deserialize)]
pub enum StandardScript {
    P2PKH(hash160::Hash),
    P2SH(hash160::Hash),
    P2WPKH(hash160::Hash),
    P2WSH(sha256::Hash),
    P2TR(XOnlyPublicKey),
}

impl StandardScript {
    pub fn from_address(address: &Address) -> Option<Self> {
        if let Some(hash) = address.pubkey_hash() {
            return Some(Self::P2PKH(hash.to_raw_hash()));
        }

        if let Some(hash) = address.script_hash() {
            return Some(Self::P2SH(hash.to_raw_hash()));
        }

        let program = address.witness_program()?;

        if program.is_p2wpkh() {
            return Some(Self::P2WPKH(
                hash160::Hash::from_slice(program.program().as_bytes())
                    .expect("Witness program is 20 bytes"),
            ));
        }

        if program.is_p2wsh() {
            return Some(Self::P2WSH(
                sha256::Hash::from_slice(program.program().as_bytes())
                    .expect("Witness program is 32 bytes"),
            ));
        }

        if program.is_p2tr() {
            return Some(Self::P2TR(
                XOnlyPublicKey::from_slice(program.program().as_bytes())
                    .expect("Witness program is 32 bytes"),
            ));
        }

        None
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        match self {
            Self::P2PKH(hash) => ScriptBuf::new_p2pkh(&PubkeyHash::from_raw_hash(*hash)),
            Self::P2SH(hash) => ScriptBuf::new_p2sh(&ScriptHash::from_raw_hash(*hash)),
            Self::P2WPKH(hash) => ScriptBuf::new_p2wpkh(&WPubkeyHash::from_raw_hash(*hash)),
            Self::P2WSH(hash) => ScriptBuf::new_p2wsh(&WScriptHash::from_raw_hash(*hash)),
            Self::P2TR(pk) => ScriptBuf::new_p2tr_tweaked(pk.dangerous_assume_tweaked()),
        }
    }
}

#[cfg(test)]
fn assert_standard_script_roundtrip(addr: &str, variant: fn(&StandardScript) -> bool) {
    let address = addr
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .expect("Failed to parse address")
        .require_network(bitcoin::Network::Bitcoin)
        .expect("Wrong network");

    let script = StandardScript::from_address(&address)
        .expect("Failed to convert address to StandardScript");

    assert!(variant(&script), "Unexpected StandardScript variant");

    assert_eq!(address.script_pubkey(), script.script_pubkey());
}

#[test]
fn test_standard_script_p2pkh() {
    assert_standard_script_roundtrip("1QJVDzdqb1VpbDK7uDeyVXy9mR27CJiyhY", |s| {
        matches!(s, StandardScript::P2PKH(..))
    });
}

#[test]
fn test_standard_script_p2sh() {
    assert_standard_script_roundtrip("33iFwdLuRpW1uK1RTRqsoi8rR4NpDzk66k", |s| {
        matches!(s, StandardScript::P2SH(..))
    });
}

#[test]
fn test_standard_script_p2wpkh() {
    assert_standard_script_roundtrip("bc1qvzvkjn4q3nszqxrv3nraga2r822xjty3ykvkuw", |s| {
        matches!(s, StandardScript::P2WPKH(..))
    });
}

#[test]
fn test_standard_script_p2wsh() {
    assert_standard_script_roundtrip(
        "bc1qwqdg6squsna38e46795at95yu9atm8azzmyvckulcc7kytlcckxswvvzej",
        |s| matches!(s, StandardScript::P2WSH(..)),
    );
}

#[test]
fn test_standard_script_p2tr() {
    assert_standard_script_roundtrip(
        "bc1p5cyxnuxmeuwuvkwfem96lqzszd02n6xdcjrs20cac6yqjjwudpxqkedrcr",
        |s| matches!(s, StandardScript::P2TR(..)),
    );
}

#[test]
fn test_standard_script_unknown_witness_version() {
    let address = "bc1pw508d6qejxtdg4y5r3zarvary0c5xw7kw508d6qejxtdg4y5r3zarvary0c5xw7kt5nd6y"
        .parse::<bitcoin::Address<bitcoin::address::NetworkUnchecked>>()
        .expect("Failed to parse address")
        .require_network(bitcoin::Network::Bitcoin)
        .expect("Wrong network");

    assert!(StandardScript::from_address(&address).is_none());
}
