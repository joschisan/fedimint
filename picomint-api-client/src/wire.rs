//! Static wire enums for the fixed module set: mintv2 + lnv2 + walletv2.

use std::fmt;

use picomint_core::core::ModuleInstanceId;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_lnv2_common::{
    LightningConsensusItem, LightningInput, LightningInputError, LightningOutput,
    LightningOutputError,
};
use picomint_mintv2_common::{
    MintConsensusItem, MintInput, MintInputError, MintOutput, MintOutputError,
};
use picomint_walletv2_common::{
    WalletConsensusItem, WalletInput, WalletInputError, WalletOutput, WalletOutputError,
};
use thiserror::Error;

/// Fixed instance ids assigned to the three canonical modules. These retain
/// compatibility with dashboards/debug tooling that still surface an instance
/// id even after the dynamic registry is ripped.
pub const MINT_INSTANCE_ID: ModuleInstanceId = 0;
pub const LN_INSTANCE_ID: ModuleInstanceId = 1;
pub const WALLET_INSTANCE_ID: ModuleInstanceId = 2;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum Input {
    Mint(MintInput),
    Ln(LightningInput),
    Wallet(WalletInput),
}

impl Input {
    pub fn module_instance_id(&self) -> ModuleInstanceId {
        match self {
            Self::Mint(_) => MINT_INSTANCE_ID,
            Self::Ln(_) => LN_INSTANCE_ID,
            Self::Wallet(_) => WALLET_INSTANCE_ID,
        }
    }
}

impl fmt::Display for Input {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mint(v) => v.fmt(f),
            Self::Ln(v) => v.fmt(f),
            Self::Wallet(v) => v.fmt(f),
        }
    }
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

impl Output {
    pub fn module_instance_id(&self) -> ModuleInstanceId {
        match self {
            Self::Mint(_) => MINT_INSTANCE_ID,
            Self::Ln(_) => LN_INSTANCE_ID,
            Self::Wallet(_) => WALLET_INSTANCE_ID,
        }
    }
}

impl fmt::Display for Output {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mint(v) => v.fmt(f),
            Self::Ln(v) => v.fmt(f),
            Self::Wallet(v) => v.fmt(f),
        }
    }
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

impl ModuleConsensusItem {
    pub fn module_instance_id(&self) -> ModuleInstanceId {
        match self {
            Self::Mint(_) => MINT_INSTANCE_ID,
            Self::Ln(_) => LN_INSTANCE_ID,
            Self::Wallet(_) => WALLET_INSTANCE_ID,
        }
    }

    pub fn module_kind(&self) -> &'static str {
        match self {
            Self::Mint(_) => "mintv2",
            Self::Ln(_) => "lnv2",
            Self::Wallet(_) => "walletv2",
        }
    }
}

impl fmt::Display for ModuleConsensusItem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Mint(v) => v.fmt(f),
            Self::Ln(v) => v.fmt(f),
            Self::Wallet(v) => v.fmt(f),
        }
    }
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
