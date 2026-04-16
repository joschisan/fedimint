//! Static wire enums for the fixed module set: mintv2 + lnv2 + walletv2.
//!
//! Subsequent commits rewire `Transaction`, the consensus engine, API routing,
//! and DKG to dispatch on these enums instead of dynamic `DynInput`/`DynOutput`/
//! `DynModuleConsensusItem`.

#![allow(dead_code)]

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_lnv2_common::{
    LightningConsensusItem, LightningInput, LightningInputError, LightningOutput,
    LightningOutputError,
};
use fedimint_mintv2_common::{
    MintConsensusItem, MintInput, MintInputError, MintOutput, MintOutputError,
};
use fedimint_walletv2_common::{
    WalletConsensusItem, WalletInput, WalletInputError, WalletOutput, WalletOutputError,
};
use thiserror::Error;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum Input {
    Mint(MintInput),
    Ln(LightningInput),
    Wallet(WalletInput),
}

impl From<MintInput> for Input {
    fn from(v: MintInput) -> Self {
        Self::Mint(v)
    }
}

impl From<LightningInput> for Input {
    fn from(v: LightningInput) -> Self {
        Self::Ln(v)
    }
}

impl From<WalletInput> for Input {
    fn from(v: WalletInput) -> Self {
        Self::Wallet(v)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum Output {
    Mint(MintOutput),
    Ln(LightningOutput),
    Wallet(WalletOutput),
}

impl From<MintOutput> for Output {
    fn from(v: MintOutput) -> Self {
        Self::Mint(v)
    }
}

impl From<LightningOutput> for Output {
    fn from(v: LightningOutput) -> Self {
        Self::Ln(v)
    }
}

impl From<WalletOutput> for Output {
    fn from(v: WalletOutput) -> Self {
        Self::Wallet(v)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum ModuleConsensusItem {
    Mint(MintConsensusItem),
    Ln(LightningConsensusItem),
    Wallet(WalletConsensusItem),
}

impl From<MintConsensusItem> for ModuleConsensusItem {
    fn from(v: MintConsensusItem) -> Self {
        Self::Mint(v)
    }
}

impl From<LightningConsensusItem> for ModuleConsensusItem {
    fn from(v: LightningConsensusItem) -> Self {
        Self::Ln(v)
    }
}

impl From<WalletConsensusItem> for ModuleConsensusItem {
    fn from(v: WalletConsensusItem) -> Self {
        Self::Wallet(v)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Error)]
pub enum InputError {
    #[error("Mint input error: {0}")]
    Mint(MintInputError),
    #[error("Lightning input error: {0}")]
    Ln(LightningInputError),
    #[error("Wallet input error: {0}")]
    Wallet(WalletInputError),
}

impl From<MintInputError> for InputError {
    fn from(v: MintInputError) -> Self {
        Self::Mint(v)
    }
}

impl From<LightningInputError> for InputError {
    fn from(v: LightningInputError) -> Self {
        Self::Ln(v)
    }
}

impl From<WalletInputError> for InputError {
    fn from(v: WalletInputError) -> Self {
        Self::Wallet(v)
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable, Error)]
pub enum OutputError {
    #[error("Mint output error: {0}")]
    Mint(MintOutputError),
    #[error("Lightning output error: {0}")]
    Ln(LightningOutputError),
    #[error("Wallet output error: {0}")]
    Wallet(WalletOutputError),
}

impl From<MintOutputError> for OutputError {
    fn from(v: MintOutputError) -> Self {
        Self::Mint(v)
    }
}

impl From<LightningOutputError> for OutputError {
    fn from(v: LightningOutputError) -> Self {
        Self::Ln(v)
    }
}

impl From<WalletOutputError> for OutputError {
    fn from(v: WalletOutputError) -> Self {
        Self::Wallet(v)
    }
}
