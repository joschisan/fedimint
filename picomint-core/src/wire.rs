//! Static wire enums for the fixed module set: mint + ln + wallet.

use std::fmt;

use crate::core::ModuleKind;
use crate::ln::{
    LightningConsensusItem, LightningInput, LightningInputError, LightningOutput,
    LightningOutputError,
};
use crate::mint::{MintConsensusItem, MintInput, MintInputError, MintOutput, MintOutputError};
use crate::wallet::{
    WalletConsensusItem, WalletInput, WalletInputError, WalletOutput, WalletOutputError,
};
use picomint_encoding::{Decodable, Encodable};
use thiserror::Error;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum Input {
    Mint(MintInput),
    Ln(LightningInput),
    Wallet(WalletInput),
}

impl Input {
    pub fn module_kind(&self) -> ModuleKind {
        match self {
            Self::Mint(_) => ModuleKind::Mint,
            Self::Ln(_) => ModuleKind::Ln,
            Self::Wallet(_) => ModuleKind::Wallet,
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
    pub fn module_kind(&self) -> ModuleKind {
        match self {
            Self::Mint(_) => ModuleKind::Mint,
            Self::Ln(_) => ModuleKind::Ln,
            Self::Wallet(_) => ModuleKind::Wallet,
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
    pub fn module_kind(&self) -> ModuleKind {
        match self {
            Self::Mint(_) => ModuleKind::Mint,
            Self::Ln(_) => ModuleKind::Ln,
            Self::Wallet(_) => ModuleKind::Wallet,
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
