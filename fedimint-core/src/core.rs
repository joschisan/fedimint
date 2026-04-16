//! Fedimint Core API (common) module interface
//!
//! Fedimint supports externally implemented modules.
//!
//! This (Rust) module defines common interoperability types
//! and functionality that is used on both client and sever side.
use core::fmt;
use std::borrow::Cow;
use std::fmt::{Debug, Display, Formatter};
use std::str::FromStr;

use bitcoin::hashes::{Hash, sha256};
use fedimint_core::encoding::{Decodable, Encodable};
use rand::RngCore;
use serde::{Deserialize, Deserializer, Serialize};

/// Unique identifier for one semantic, correlatable operation.
///
/// The concept of *operations* is used to avoid losing privacy while being as
/// efficient as possible with regards to network requests.
///
/// For Fedimint transactions to be private users need to communicate with the
/// federation using an anonymous communication network. If each API request was
/// done in a way that it cannot be correlated to any other API request we would
/// achieve privacy, but would reduce efficiency. E.g. on Tor we would need to
/// open a new circuit for every request and open a new web socket connection.
///
/// Fortunately we do not need to do that to maintain privacy. Many API requests
/// and transactions can be correlated by the federation anyway, in these cases
/// it does not make any difference to re-use the same network connection. All
/// requests, transactions, state machines that are connected from the
/// federation's point of view anyway are grouped together as one *operation*.
///
/// # Choice of Operation ID
///
/// In cases where an operation is created by a new transaction that's being
/// submitted the transaction's ID can be used as operation ID. If there is no
/// transaction related to it, it should be generated randomly. Since it is a
/// 256bit value collisions are impossible for all intents and purposes.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Encodable, Decodable, PartialOrd, Ord)]
pub struct OperationId(pub [u8; 32]);

pub struct OperationIdFullFmt<'a>(&'a OperationId);
pub struct OperationIdShortFmt<'a>(&'a OperationId);

impl OperationId {
    /// Generate random [`OperationId`]
    pub fn new_random() -> Self {
        let mut rng = rand::thread_rng();
        let mut bytes = [0u8; 32];
        rng.fill_bytes(&mut bytes);
        Self(bytes)
    }

    pub fn from_encodable<E: Encodable>(encodable: &E) -> Self {
        Self(encodable.consensus_hash::<sha256::Hash>().to_byte_array())
    }

    pub fn fmt_short(&'_ self) -> OperationIdShortFmt<'_> {
        OperationIdShortFmt(self)
    }
    pub fn fmt_full(&'_ self) -> OperationIdFullFmt<'_> {
        OperationIdFullFmt(self)
    }
}

impl Display for OperationIdShortFmt<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        fedimint_core::format_hex(&self.0.0[0..4], f)?;
        f.write_str("_")?;
        fedimint_core::format_hex(&self.0.0[28..], f)?;
        Ok(())
    }
}

impl Display for OperationIdFullFmt<'_> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        fedimint_core::format_hex(&self.0.0, f)
    }
}

impl Debug for OperationId {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "OperationId({})", self.fmt_short())
    }
}

impl FromStr for OperationId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let bytes: [u8; 32] = hex::FromHex::from_hex(s)?;
        Ok(Self(bytes))
    }
}

impl Serialize for OperationId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            serializer.serialize_str(&self.fmt_full().to_string())
        } else {
            serializer.serialize_bytes(&self.0)
        }
    }
}

impl<'de> Deserialize<'de> for OperationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        if deserializer.is_human_readable() {
            let s = String::deserialize(deserializer)?;
            let operation_id = Self::from_str(&s)
                .map_err(|e| serde::de::Error::custom(format!("invalid operation id: {e}")))?;
            Ok(operation_id)
        } else {
            let bytes: [u8; 32] = <[u8; 32]>::deserialize(deserializer)?;
            Ok(Self(bytes))
        }
    }
}

crate::consensus_key!(OperationId);

/// Module instance ID
///
/// This value uniquely identifies a single instance of a module in a
/// federation.
///
/// In case a single [`ModuleKind`] is instantiated twice (rare, but possible),
/// each instance will have a different id.
///
/// Note: We have used this type differently before, assuming each `u16`
/// uniquly identifies a type of module in question. This function will move
/// to a `ModuleKind` type which only identifies type of a module (mint vs
/// wallet vs ln, etc)
// TODO: turn in a newtype
pub type ModuleInstanceId = u16;

/// Special IDs we use for global dkg
pub const MODULE_INSTANCE_ID_GLOBAL: u16 = u16::MAX;

/// A type of a module
///
/// This is a short string that identifies type of a module.
/// Authors of 3rd party modules are free to come up with a string,
/// long enough to avoid conflicts with similar modules.
#[derive(PartialEq, Eq, Clone, PartialOrd, Ord, Serialize, Deserialize, Encodable, Decodable)]
pub struct ModuleKind(Cow<'static, str>);

impl ModuleKind {
    pub fn clone_from_str(s: &str) -> Self {
        Self(Cow::from(s.to_owned()))
    }

    pub const fn from_static_str(s: &'static str) -> Self {
        Self(Cow::Borrowed(s))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ModuleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl fmt::Debug for ModuleKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        std::fmt::Display::fmt(&self.0, f)
    }
}

impl From<&'static str> for ModuleKind {
    fn from(val: &'static str) -> Self {
        Self::from_static_str(val)
    }
}
