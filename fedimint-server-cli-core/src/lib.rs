use fedimint_core::module::audit::AuditSummary;
pub use fedimint_server_core::dashboard_ui::SetupStatus;
use serde::{Deserialize, Serialize};

// Setup routes
pub const ROUTE_SETUP_STATUS: &str = "/setup/status";
pub const ROUTE_SETUP_SET_LOCAL_PARAMS: &str = "/setup/set-local-params";
pub const ROUTE_SETUP_ADD_PEER: &str = "/setup/add-peer";
pub const ROUTE_SETUP_START_DKG: &str = "/setup/start-dkg";

// Dashboard routes
pub const ROUTE_INVITE: &str = "/invite";
pub const ROUTE_AUDIT: &str = "/audit";

// Module routes
pub const ROUTE_MODULE_WALLET_TOTAL_VALUE: &str = "/module/walletv2/total-value";
pub const ROUTE_MODULE_WALLET_BLOCK_COUNT: &str = "/module/walletv2/block-count";
pub const ROUTE_MODULE_WALLET_FEERATE: &str = "/module/walletv2/feerate";
pub const ROUTE_MODULE_WALLET_PENDING_TX_CHAIN: &str = "/module/walletv2/pending-tx-chain";
pub const ROUTE_MODULE_WALLET_TX_CHAIN: &str = "/module/walletv2/tx-chain";
pub const ROUTE_MODULE_LNV2_GATEWAY_ADD: &str = "/module/lnv2/gateway/add";
pub const ROUTE_MODULE_LNV2_GATEWAY_REMOVE: &str = "/module/lnv2/gateway/remove";
pub const ROUTE_MODULE_LNV2_GATEWAY_LIST: &str = "/module/lnv2/gateway/list";

// --- /setup/status ---
// Response: SetupStatus (re-exported from fedimint-server-core)

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

// --- /module/walletv2/total-value ---

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletTotalValueResponse {
    pub total_value_sats: Option<u64>,
}

// --- /module/walletv2/block-count ---

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletBlockCountResponse {
    pub block_count: u64,
}

// --- /module/walletv2/feerate ---

#[derive(Debug, Serialize, Deserialize)]
pub struct WalletFeerateResponse {
    pub sats_per_vbyte: Option<u64>,
}

// --- /module/lnv2/gateway/* ---

#[derive(Debug, Serialize, Deserialize)]
pub struct Lnv2GatewayRequest {
    pub url: String,
}
