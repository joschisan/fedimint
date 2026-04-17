#![deny(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

//! # Lightning Module
//!
//! This module allows to atomically and trustlessly (in the federated trust
//! model) interact with the Lightning network through a Lightning gateway.

pub mod config;
pub mod contracts;
pub mod endpoint_constants;
pub mod gateway_api;
pub mod lnurl;
pub mod tweak;

use bitcoin::hashes::sha256;
use bitcoin::secp256k1::schnorr::Signature;
use config::LightningConfigConsensus;
pub use gateway_connection::GatewayApi;
use picomint_core::core::ModuleKind;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::module::{CommonModuleInit, ModuleConsensusVersion};
use picomint_core::module::ModuleCommon;
use picomint_core::{Amount, OutPoint};
mod gateway_connection;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tpe::AggregateDecryptionKey;

use crate::contracts::{IncomingContract, OutgoingContract};

#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize)]
pub enum Bolt11InvoiceDescription {
    Direct(String),
    Hash(sha256::Hash),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Decodable, Encodable)]
pub enum LightningInvoice {
    Bolt11(Bolt11Invoice),
}

pub const KIND: ModuleKind = ModuleKind::Ln;
pub const MODULE_CONSENSUS_VERSION: ModuleConsensusVersion = ModuleConsensusVersion::new(1, 0);

/// Minimum contract amount to ensure the incoming contract can be claimed
/// without additional funds.
pub const MINIMUM_INCOMING_CONTRACT_AMOUNT: Amount = Amount::from_sats(5);

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct ContractId(pub sha256::Hash);

picomint_redb::consensus_key!(ContractId);

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub enum LightningInput {
    Outgoing(OutPoint, OutgoingWitness),
    Incoming(OutPoint, AggregateDecryptionKey),
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub enum OutgoingWitness {
    Claim([u8; 32]),
    Refund,
    Cancel(Signature),
}

impl std::fmt::Display for LightningInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LightningInput",)
    }
}

#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub enum LightningOutput {
    Outgoing(OutgoingContract),
    Incoming(IncomingContract),
}

impl std::fmt::Display for LightningOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LightningOutput")
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Error, Encodable, Decodable)]
pub enum LightningInputError {
    #[error("No contract found for given ContractId")]
    UnknownContract,
    #[error("The preimage is invalid")]
    InvalidPreimage,
    #[error("The contracts locktime has passed")]
    Expired,
    #[error("The contracts locktime has not yet passed")]
    NotExpired,
    #[error("The aggregate decryption key is invalid")]
    InvalidDecryptionKey,
    #[error("The forfeit signature is invalid")]
    InvalidForfeitSignature,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Error, Encodable, Decodable)]
pub enum LightningOutputError {
    #[error("The contract is invalid")]
    InvalidContract,
    #[error("The contract is expired")]
    ContractExpired,
}

#[derive(Debug, Clone, Hash, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub enum LightningConsensusItem {
    BlockCountVote(u64),
    UnixTimeVote(u64),
}

impl std::fmt::Display for LightningConsensusItem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LightningConsensusItem::BlockCountVote(c) => {
                write!(f, "LNv2 Block Count {c}")
            }
            LightningConsensusItem::UnixTimeVote(t) => {
                write!(f, "LNv2 Unix Time {t}")
            }
        }
    }
}

#[derive(Debug)]
pub struct LightningCommonInit;

impl CommonModuleInit for LightningCommonInit {
    const CONSENSUS_VERSION: ModuleConsensusVersion = MODULE_CONSENSUS_VERSION;
    const KIND: ModuleKind = KIND;

    type ClientConfig = LightningConfigConsensus;
}

pub struct LightningModuleTypes;

impl ModuleCommon for LightningModuleTypes {
    type ClientConfig = LightningConfigConsensus;
    type Input = LightningInput;
    type Output = LightningOutput;
    type ConsensusItem = LightningConsensusItem;
    type InputError = LightningInputError;
    type OutputError = LightningOutputError;
}
