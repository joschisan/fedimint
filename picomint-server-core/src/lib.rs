//! Shared server-side traits and helpers
//!
//! This crate is the narrow shared layer between `picomint-server-daemon` and
//! the three concrete server-side module implementations
//! (`picomint-mint-server`, `picomint-ln-server`,
//! `picomint-wallet-server`). It defines the `ServerModule` trait (which each
//! module implements) and the DKG-time `PeerHandleOps` trait.

pub mod config;

use std::fmt::Debug;

use picomint_core::module::audit::Audit;
use picomint_core::module::{
    ApiError, ApiRequestErased, InputMeta, ModuleCommon, TransactionItemAmounts,
};
use picomint_core::{InPoint, OutPoint, PeerId};
use picomint_redb::{ReadTxRef, WriteTxRef};
use serde::{Deserialize, Serialize};

/// P2P connection status for a peer. `None` in a status channel means the peer
/// is currently disconnected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct P2PConnectionStatus {
    /// Round-trip time (only available for iroh connections)
    pub rtt: Option<std::time::Duration>,
}

#[async_trait::async_trait]
pub trait ServerModule: Debug + Sized {
    type Common: ModuleCommon;

    /// This module's contribution to the next consensus proposal. Only called
    /// once every few seconds; consensus items are not meant to be latency
    /// critical.
    async fn consensus_proposal(
        &self,
        dbtx: &ReadTxRef<'_>,
    ) -> Vec<<Self::Common as ModuleCommon>::ConsensusItem>;

    /// Process a single consensus item. Return `Ok` only if it changes state;
    /// return an error for merely redundant items so they get purged from
    /// federation history.
    async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: <Self::Common as ModuleCommon>::ConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()>;

    /// Try to spend a transaction input. On failure the dbtx is rolled back.
    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &<Self::Common as ModuleCommon>::Input,
        in_point: InPoint,
    ) -> Result<InputMeta, <Self::Common as ModuleCommon>::InputError>;

    /// Try to create an output.
    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &<Self::Common as ModuleCommon>::Output,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, <Self::Common as ModuleCommon>::OutputError>;

    /// Sum assets and liabilities of the module into the audit accumulator.
    async fn audit(&self, dbtx: &WriteTxRef<'_>, audit: &mut Audit);

    /// Dispatch a client API request to this module. Decode the request
    /// params, run the matching handler, and return the consensus-encoded
    /// response. Unknown methods return `ApiError::not_found`.
    async fn handle_api(
        &self,
        method: &str,
        req: ApiRequestErased,
    ) -> Result<Vec<u8>, ApiError>;
}

/// Dispatch helper for [`ServerModule::handle_api`] arms.
///
/// `handler!(fn_name, self, req).await` expands to an `async` block that
/// decodes `req` into the parameter type of `rpc::fn_name`, calls
/// `rpc::fn_name(self, param).await`, and consensus-encodes the response.
/// Each module is expected to have a `mod rpc` submodule with one
/// `async fn name(module: &Self, param: P) -> Result<R, ApiError>` per
/// endpoint. Mirrors the puncture-style dispatch pattern.
#[macro_export]
macro_rules! handler {
    ($func:ident, $self:expr, $req:expr) => {
        async move {
            let param = $req
                .to_typed()
                .map_err(|e| ::picomint_core::module::ApiError::bad_request(e.to_string()))?;
            let resp = rpc::$func($self, param).await?;
            ::std::result::Result::Ok(
                ::picomint_encoding::Encodable::consensus_encode_to_vec(&resp),
            )
        }
    };
}
