#![deny(clippy::pedantic, clippy::nursery)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::cognitive_complexity)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::future_not_send)]
#![allow(clippy::missing_const_for_fn)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::redundant_pub_crate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::similar_names)]
#![allow(clippy::transmute_ptr_to_ptr)]
#![allow(clippy::unsafe_derive_deserialize)]

//! Picomint Core library
//!
//! `picomint-core` contains commonly used types, utilities and primitives,
//! shared between both client and server code.
//!
//! Things that are server-side only typically live in `picomint-server`, and
//! client-side only in `picomint-client`.
//!
//! ### Wasm support
//!
//! All code in `picomint-core` needs to compile on Wasm, and `picomint-core`
//! includes helpers and wrappers around non-wasm-safe utitlies.
//!
//! In particular:
//!
//! * [`picomint_core::task`] for task spawning and control
//! * [`picomint_core::time`] for time-related operations

extern crate self as picomint_core;

pub use amount::*;
/// Mostly re-exported for [`Decodable`] macros.
pub use anyhow;
pub use bitcoin::hashes::Hash as BitcoinHash;
pub use peer_id::*;
use serde::{Deserialize, Serialize};
use std::fmt::Debug;
pub use {bitcoin, hex, secp256k1};

use picomint_encoding::{Decodable, Encodable};

/// Bitcoin amount types
mod amount;
/// Fibonacci backoff policies for retry loops.
pub mod backoff;
/// Federation configuration
pub mod config;
/// Fundamental types
pub mod core;
pub mod endpoint_constants;
/// Common environment variables
pub mod envs;
/// Federation invite code
pub mod invite_code;
/// Lightning module wire types / helpers (shared between client and server).
pub mod ln;
/// Mint module wire types / helpers (shared between client and server).
pub mod mint;
/// Extendable module sysystem
pub mod module;
/// `PeerId` type
mod peer_id;
/// Consensus session outcome types (AcceptedItem, SessionOutcome, …).
pub mod secret;

pub mod session_outcome;
/// Task handling, including wasm safe logic
pub mod task;
/// Time handling, wasm safe functionality
pub mod time;
/// Wire-level Transaction and ConsensusItem types.
pub mod transaction;
/// General purpose utilities
pub mod util;
/// Wallet module wire types / helpers (shared between client and server).
pub mod wallet;
/// Static wire enums over the fixed module set.
pub mod wire;

/// Atomic BFT unit containing consensus items

// It's necessary to wrap `hash_newtype!` in a module because the generated code
// references a module called "core", but we export a conflicting module in this
// file.
mod txid {
    use bitcoin::hashes::hash_newtype;
    use bitcoin::hashes::sha256::Hash as Sha256;

    hash_newtype!(
        /// A transaction id for peg-ins, peg-outs and reissuances
        pub struct TransactionId(Sha256);
    );
}
pub use txid::TransactionId;

impl redb::Value for TransactionId {
    type SelfType<'a>
        = TransactionId
    where
        Self: 'a;
    type AsBytes<'a>
        = [u8; 32]
    where
        Self: 'a;
    fn fixed_width() -> Option<usize> {
        Some(32)
    }
    fn from_bytes<'a>(data: &'a [u8]) -> Self
    where
        Self: 'a,
    {
        use bitcoin::hashes::Hash as _;
        let bytes: [u8; 32] = data.try_into().expect("sha256 hash is always 32 bytes");
        Self::from_byte_array(bytes)
    }
    fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
    where
        Self: 'b,
    {
        use bitcoin::hashes::Hash as _;
        value.to_byte_array()
    }
    fn type_name() -> redb::TypeName {
        redb::TypeName::new("picomint::TransactionId")
    }
}

impl redb::Key for TransactionId {
    fn compare(data1: &[u8], data2: &[u8]) -> std::cmp::Ordering {
        data1.cmp(data2)
    }
}

/// `InPoint` represents a globally unique input in a transaction
///
/// Hence, a transaction ID and the input index is required.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    Serialize,
    Encodable,
    Decodable,
)]
pub struct InPoint {
    /// The referenced transaction ID
    pub txid: TransactionId,
    /// As a transaction may have multiple inputs, this refers to the index of
    /// the input in a transaction
    pub in_idx: u64,
}

impl std::fmt::Display for InPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.txid, self.in_idx)
    }
}

/// `OutPoint` represents a globally unique output in a transaction
///
/// Hence, a transaction ID and the output index is required.
#[derive(
    Debug,
    Clone,
    Copy,
    Eq,
    PartialEq,
    PartialOrd,
    Ord,
    Hash,
    Deserialize,
    Serialize,
    Encodable,
    Decodable,
)]
pub struct OutPoint {
    /// The referenced transaction ID
    pub txid: TransactionId,
    /// As a transaction may have multiple outputs, this refers to the index of
    /// the output in a transaction
    pub out_idx: u64,
}

impl std::fmt::Display for OutPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.txid, self.out_idx)
    }
}

picomint_redb::consensus_key!(OutPoint);

impl Encodable for TransactionId {
    fn consensus_encode<W: std::io::Write>(&self, writer: &mut W) -> std::io::Result<()> {
        let bytes = &self[..];
        writer.write_all(bytes)?;
        Ok(())
    }
}

impl Decodable for TransactionId {
    fn consensus_decode<R: std::io::Read>(r: &mut R) -> std::io::Result<Self> {
        let mut bytes = [0u8; 32];
        r.read_exact(&mut bytes)?;
        Ok(Self::from_byte_array(bytes))
    }
}
