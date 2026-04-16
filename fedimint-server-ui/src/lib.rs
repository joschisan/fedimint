//! Server-side admin web UI for `fedimintd`.
//!
//! The UI runs in two phases:
//!
//! - Setup UI (before the federation is configured). Served by
//!   [`setup::router`] and driven by [`ISetupApi`].
//! - Dashboard UI (once the federation is running). Served by
//!   [`dashboard::router`] and driven by [`IDashboardApi`], which exposes the
//!   three canonical module instances as typed accessors (`mint`, `ln`,
//!   `wallet`) so the dashboard can speak directly to them.

pub mod dashboard;
pub mod setup;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use fedimint_api_client::session_outcome::SessionStatusV2;
use fedimint_core::PeerId;
use fedimint_core::module::ApiAuth;
use fedimint_core::module::audit::AuditSummary;
use fedimint_core::net::auth::GuardianAuthToken;
use fedimint_core::util::SafeUrl;
pub use fedimint_server_core::{P2PConnectionStatus, ServerBitcoinRpcStatus};
use serde::{Deserialize, Serialize};

// Common route constants
pub const DOWNLOAD_BACKUP_ROUTE: &str = "/download-backup";

pub type DynDashboardApi = Arc<dyn IDashboardApi + Send + Sync + 'static>;
pub type DynSetupApi = Arc<dyn ISetupApi + Send + Sync + 'static>;

/// Archive of all the guardian config files that can be used to recover a lost
/// guardian node.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct GuardianConfigBackup {
    #[serde(with = "fedimint_core::hex::serde")]
    pub tar_archive_bytes: Vec<u8>,
}

/// The state of the server returned to the setup CLI/UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetupStatus {
    /// Waiting for guardian to set the local parameters
    AwaitingLocalParams,
    /// Sharing the connection codes with our peers
    SharingConnectionCodes,
    /// Consensus is running
    ConsensusIsRunning,
}

/// Interface for guardian dashboard API in a running federation.
#[async_trait]
pub trait IDashboardApi {
    async fn auth(&self) -> ApiAuth;
    async fn guardian_id(&self) -> PeerId;
    async fn guardian_names(&self) -> BTreeMap<PeerId, String>;
    async fn federation_name(&self) -> String;
    async fn session_count(&self) -> u64;
    async fn get_session_status(&self, session_idx: u64) -> SessionStatusV2;
    async fn consensus_ord_latency(&self) -> Option<Duration>;
    async fn p2p_connection_status(&self) -> BTreeMap<PeerId, Option<P2PConnectionStatus>>;
    async fn federation_invite_code(&self) -> String;
    async fn federation_audit(&self) -> AuditSummary;
    async fn bitcoin_rpc_url(&self) -> SafeUrl;
    async fn bitcoin_rpc_status(&self) -> Option<ServerBitcoinRpcStatus>;
    async fn download_guardian_config_backup(
        &self,
        guardian_auth: &GuardianAuthToken,
    ) -> GuardianConfigBackup;

    /// Typed accessor for the mint module instance.
    fn mint(&self) -> &fedimint_mintv2_server::Mint;

    /// Typed accessor for the lightning module instance.
    fn lightning(&self) -> &fedimint_lnv2_server::Lightning;

    /// Typed accessor for the wallet module instance.
    fn wallet(&self) -> &fedimint_walletv2_server::Wallet;

    async fn fedimintd_version(&self) -> String;

    fn into_dyn(self) -> DynDashboardApi
    where
        Self: Sized + Send + Sync + 'static,
    {
        Arc::new(self)
    }
}

/// Interface driving the setup UI / CLI while config generation is running.
#[async_trait]
pub trait ISetupApi {
    async fn setup_code(&self) -> Option<String>;
    async fn guardian_name(&self) -> Option<String>;
    async fn auth(&self) -> ApiAuth;
    async fn connected_peers(&self) -> Vec<String>;
    async fn reset_setup_codes(&self);

    async fn set_local_parameters(
        &self,
        name: String,
        federation_name: Option<String>,
        federation_size: Option<u32>,
    ) -> anyhow::Result<String>;

    async fn add_peer_setup_code(&self, info: String) -> anyhow::Result<String>;
    async fn start_dkg(&self) -> anyhow::Result<()>;
    async fn federation_size(&self) -> Option<u32>;
    async fn cfg_federation_name(&self) -> Option<String>;

    fn into_dyn(self) -> DynSetupApi
    where
        Self: Sized + Send + Sync + 'static,
    {
        Arc::new(self)
    }
}
