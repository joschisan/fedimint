use picomint_core::module::audit::AuditSummary;
use serde::{Deserialize, Serialize};

/// Filename of the guardian's admin CLI Unix socket, inside `DATA_DIR`.
/// The daemon binds and the CLI connects at `{DATA_DIR}/{CLI_SOCKET_FILENAME}`.
pub const CLI_SOCKET_FILENAME: &str = "cli.sock";

/// Status of the setup flow — mirrors `picomint_server_ui::SetupStatus`
/// as a CLI-consumed copy so `picomint-server-cli` doesn't need to pull in the
/// server-ui crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SetupStatus {
    AwaitingLocalParams,
    SharingConnectionCodes,
}

// Setup routes
pub const ROUTE_SETUP_STATUS: &str = "/setup/status";
pub const ROUTE_SETUP_SET_LOCAL_PARAMS: &str = "/setup/set-local-params";
pub const ROUTE_SETUP_ADD_PEER: &str = "/setup/add-peer";
pub const ROUTE_SETUP_START_DKG: &str = "/setup/start-dkg";
pub const ROUTE_SETUP_RESTORE: &str = "/setup/restore";

// Dashboard routes
pub const ROUTE_INVITE: &str = "/invite";
pub const ROUTE_AUDIT: &str = "/audit";
pub const ROUTE_CONFIG: &str = "/config";
pub const ROUTE_SESSION_COUNT: &str = "/session-count";

// Module routes
pub const ROUTE_MODULE_WALLET_TOTAL_VALUE: &str = "/module/wallet/total-value";
pub const ROUTE_MODULE_WALLET_BLOCK_COUNT: &str = "/module/wallet/block-count";
pub const ROUTE_MODULE_WALLET_FEERATE: &str = "/module/wallet/feerate";
pub const ROUTE_MODULE_WALLET_PENDING_TX_CHAIN: &str = "/module/wallet/pending-tx-chain";
pub const ROUTE_MODULE_WALLET_TX_CHAIN: &str = "/module/wallet/tx-chain";
pub const ROUTE_MODULE_LN_GATEWAY_ADD: &str = "/module/ln/gateway/add";
pub const ROUTE_MODULE_LN_GATEWAY_REMOVE: &str = "/module/ln/gateway/remove";
pub const ROUTE_MODULE_LN_GATEWAY_LIST: &str = "/module/ln/gateway/list";

// --- /setup/status ---
// Response: SetupStatus (re-exported from picomint-server-core)

// --- /setup/set-local-params ---

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetupSetLocalParamsRequest {
    pub name: String,
    pub federation_name: Option<String>,
    pub federation_size: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetupSetLocalParamsResponse {
    pub setup_code: String,
}

// --- /setup/add-peer ---

#[derive(Debug, Serialize, Deserialize)]
pub struct SetupAddPeerRequest {
    pub setup_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetupAddPeerResponse {
    pub name: String,
}

// --- /setup/start-dkg ---
// No request/response types (unit)

// --- /invite ---

#[derive(Debug, Serialize, Deserialize)]
pub struct InviteResponse {
    pub invite_code: String,
}

// --- /audit ---

#[derive(Debug, Serialize, Deserialize)]
pub struct AuditResponse {
    pub audit: AuditSummary,
}

// --- /module/wallet/total-value ---

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletTotalValueResponse {
    pub total_value_sats: Option<u64>,
}

// --- /module/wallet/block-count ---

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletBlockCountResponse {
    pub block_count: u64,
}

// --- /module/wallet/feerate ---

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletFeerateResponse {
    pub sats_per_vbyte: Option<u64>,
}

// --- /module/ln/gateway/* ---

#[derive(Debug, Serialize, Deserialize)]
pub struct LnGatewayRequest {
    pub url: String,
}
