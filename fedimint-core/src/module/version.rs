//! Fedimint consensus and API versioning.
//!
//! ## Introduction
//!
//! Fedimint federations are expected to last and serve over time diverse set of
//! clients running on various devices and platforms with different
//! versions of the client software. To ensure broad interoperability core
//! Fedimint logic and modules use consensus and API version scheme.
//!
//! ## Definitions
//!
//! * Fedimint *component* - either a core Fedimint logic or one of the modules
//!
//! ## Consensus versions
//!
//! By definition all instances of a given component on every peer inside a
//! Federation must be running with the same consensus version at the same time.
//!
//! Each component in the Federation can only ever be in one consensus version.
//! The set of all consensus versions of each component is a part of consensus
//! config that is identical for all peers.
//!
//! The code implementing given component can however support multiple consensus
//! versions at the same time, making it possible to use the same code for
//! diverse set of Federations created at different times. The consensus
//! version to run with is passed to the code during initialization.
//!
//! The client side components need track consensus versions of each Federation
//! they use and be able to handle the currently running version of it.
//!
//! [`CoreConsensusVersion`] and [`ModuleConsensusVersion`] are used for
//! consensus versioning.
use std::cmp;

use serde::{Deserialize, Serialize};

use crate::encoding::{Decodable, Encodable};

/// Consensus version of a core server
///
/// Breaking changes in the Fedimint's core consensus require incrementing it.
///
/// See [`ModuleConsensusVersion`] for more details on how it interacts with
/// module's consensus.
#[derive(
    Debug, Copy, Clone, PartialOrd, Ord, Serialize, Deserialize, Encodable, Decodable, PartialEq, Eq,
)]
pub struct CoreConsensusVersion {
    pub major: u32,
    pub minor: u32,
}

impl CoreConsensusVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

/// Globally declared core consensus version
pub const CORE_CONSENSUS_VERSION: CoreConsensusVersion = CoreConsensusVersion::new(2, 1);

/// Consensus version of a specific module instance
///
/// Any breaking change to the module's consensus rules require incrementing the
/// major part of it.
///
/// Any backwards-compatible changes with regards to clients require
/// incrementing the minor part of it. Backwards compatible changes will
/// typically be introducing new input/output/consensus item variants that old
/// clients won't understand but can safely ignore while new clients can use new
/// functionality. It's akin to soft forks in Bitcoin.
///
/// A module instance can run only in one consensus version, which must be the
/// same (both major and minor) across all corresponding instances on other
/// nodes of the federation.
///
/// When [`CoreConsensusVersion`] changes, this can but is not requires to be
/// a breaking change for each module's [`ModuleConsensusVersion`].
///
/// For many modules it might be preferable to implement a new
/// [`fedimint_core::core::ModuleKind`] "versions" (to be implemented at the
/// time of writing this comment), and by running two instances of the module at
/// the same time (each of different `ModuleKind` version), allow users to
/// slowly migrate to a new one. This avoids complex and error-prone server-side
/// consensus-migration logic.
#[derive(
    Debug,
    Hash,
    Copy,
    Clone,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    Encodable,
    Decodable,
)]
pub struct ModuleConsensusVersion {
    pub major: u32,
    pub minor: u32,
}

impl ModuleConsensusVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

/// Api version supported by a core server or a client/server module at a given
/// [`ModuleConsensusVersion`].
///
/// Changing [`ModuleConsensusVersion`] implies resetting the api versioning.
///
/// For a client and server to be able to communicate with each other:
///
/// * The client needs API version support for the [`ModuleConsensusVersion`]
///   that the server is currently running with.
/// * Within that [`ModuleConsensusVersion`] during handshake negotiation
///   process client and server must find at least one `Api::major` version
///   where client's `minor` is lower or equal server's `major` version.
///
/// A practical module implementation needs to implement large range of version
/// backward compatibility on both client and server side to accommodate end
/// user client devices receiving updates at a pace hard to control, and
/// technical and coordination challenges of upgrading servers.
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq, Decodable, Encodable)]
pub struct ApiVersion {
    /// Major API version
    ///
    /// Each time [`ModuleConsensusVersion`] is incremented, this number (and
    /// `minor` number as well) should be reset to `0`.
    ///
    /// Should be incremented each time the API was changed in a
    /// backward-incompatible ways (while resetting `minor` to `0`).
    pub major: u32,
    /// Minor API version
    ///
    /// * For clients this means *minimum* supported minor version of the
    ///   `major` version required by client implementation
    /// * For servers this means *maximum* supported minor version of the
    ///   `major` version implemented by the server implementation
    pub minor: u32,
}

impl ApiVersion {
    pub const fn new(major: u32, minor: u32) -> Self {
        Self { major, minor }
    }
}

/// ```
/// use fedimint_core::module::ApiVersion;
/// assert!(ApiVersion { major: 3, minor: 3 } < ApiVersion { major: 4, minor: 0 });
/// assert!(ApiVersion { major: 3, minor: 3 } < ApiVersion { major: 3, minor: 5 });
/// assert!(ApiVersion { major: 3, minor: 3 } == ApiVersion { major: 3, minor: 3 });
/// ```
impl cmp::PartialOrd for ApiVersion {
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl cmp::Ord for ApiVersion {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.major
            .cmp(&other.major)
            .then(self.minor.cmp(&other.minor))
    }
}
