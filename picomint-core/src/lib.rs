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

use std::fmt::Debug;
use std::ops::{self, Range};
use std::str::FromStr;

pub use amount::*;
/// Mostly re-exported for [`Decodable`] macros.
pub use anyhow;
pub use bitcoin::hashes::Hash as BitcoinHash;
pub use peer_id::*;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
pub use {bitcoin, hex, secp256k1};

use picomint_encoding::{Decodable, Encodable};

/// Bitcoin amount types
mod amount;
/// Base 32 encoding
pub mod base32;
/// Federation configuration
pub mod config;
/// Fundamental types
pub mod core;
pub mod endpoint_constants;
/// Common environment variables
pub mod envs;
/// Federation invite code
pub mod invite_code;
/// Extendable module sysystem
pub mod module;
/// `PeerId` type
mod peer_id;
/// Task handling, including wasm safe logic
pub mod task;
/// Time handling, wasm safe functionality
pub mod time;
/// General purpose utilities
pub mod util;

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

/// Bitcoin chain identifier
///
/// This is a newtype wrapper around [`bitcoin::BlockHash`] representing the
/// block hash at height 1, which uniquely identifies a Bitcoin chain (mainnet,
/// testnet, signet, regtest, or custom networks), unlike genesis block hash
/// which is often the same for same types of networks (e.g. mutinynet vs
/// signet4).
///
/// Using a distinct type instead of raw `BlockHash` provides type safety and
/// makes the intent clearer when passing chain identifiers through APIs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Encodable, Decodable)]
pub struct ChainId(pub bitcoin::BlockHash);

impl ChainId {
    /// Create a new `ChainId` from a `BlockHash`
    pub fn new(block_hash: bitcoin::BlockHash) -> Self {
        Self(block_hash)
    }

    /// Get the inner `BlockHash`
    pub fn block_hash(&self) -> bitcoin::BlockHash {
        self.0
    }
}

impl From<bitcoin::BlockHash> for ChainId {
    fn from(block_hash: bitcoin::BlockHash) -> Self {
        Self(block_hash)
    }
}

impl From<ChainId> for bitcoin::BlockHash {
    fn from(chain_id: ChainId) -> Self {
        chain_id.0
    }
}

impl std::fmt::Display for ChainId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl FromStr for ChainId {
    type Err = bitcoin::hashes::hex::HexToArrayError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        bitcoin::BlockHash::from_str(s).map(Self)
    }
}

impl Serialize for ChainId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for ChainId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        bitcoin::BlockHash::deserialize(deserializer).map(Self)
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

/// A contiguous range of input/output indexes
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct IdxRange {
    start: u64,
    end: u64,
}

impl IdxRange {
    pub fn new_single(start: u64) -> Option<Self> {
        start.checked_add(1).map(|end| Self { start, end })
    }

    pub fn start(self) -> u64 {
        self.start
    }

    pub fn count(self) -> usize {
        self.into_iter().count()
    }

    pub fn from_inclusive(range: ops::RangeInclusive<u64>) -> Option<Self> {
        range.end().checked_add(1).map(|end| Self {
            start: *range.start(),
            end,
        })
    }
}

impl From<Range<u64>> for IdxRange {
    fn from(Range { start, end }: Range<u64>) -> Self {
        Self { start, end }
    }
}

impl IntoIterator for IdxRange {
    type Item = u64;
    type IntoIter = ops::Range<u64>;

    fn into_iter(self) -> Self::IntoIter {
        ops::Range {
            start: self.start,
            end: self.end,
        }
    }
}

/// Represents a range of output indices for a single transaction
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct OutPointRange {
    pub txid: TransactionId,
    idx_range: IdxRange,
}

impl OutPointRange {
    pub fn new(txid: TransactionId, idx_range: IdxRange) -> Self {
        Self { txid, idx_range }
    }

    pub fn new_single(txid: TransactionId, idx: u64) -> Option<Self> {
        IdxRange::new_single(idx).map(|idx_range| Self { txid, idx_range })
    }

    pub fn start_idx(self) -> u64 {
        self.idx_range.start()
    }

    pub fn out_idx_iter(self) -> impl Iterator<Item = u64> {
        self.idx_range.into_iter()
    }

    pub fn count(self) -> usize {
        self.idx_range.count()
    }

    pub fn start_out_point(self) -> OutPoint {
        OutPoint {
            txid: self.txid,
            out_idx: self.idx_range.start(),
        }
    }

    pub fn end_out_point(self) -> OutPoint {
        OutPoint {
            txid: self.txid,
            out_idx: self.idx_range.end,
        }
    }

    pub fn txid(&self) -> TransactionId {
        self.txid
    }
}

impl IntoIterator for OutPointRange {
    type Item = OutPoint;
    type IntoIter = OutPointRangeIter;

    fn into_iter(self) -> Self::IntoIter {
        OutPointRangeIter {
            txid: self.txid,
            inner: self.idx_range.into_iter(),
        }
    }
}

pub struct OutPointRangeIter {
    txid: TransactionId,
    inner: ops::Range<u64>,
}

impl Iterator for OutPointRangeIter {
    type Item = OutPoint;

    fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().map(|idx| OutPoint {
            txid: self.txid,
            out_idx: idx,
        })
    }
}

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
