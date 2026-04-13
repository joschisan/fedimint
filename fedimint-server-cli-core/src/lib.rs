pub use fedimint_server_core::dashboard_ui::SetupStatus;
use serde::{Deserialize, Serialize};

// Setup routes
pub const ROUTE_SETUP_STATUS: &str = "/setup/status";
pub const ROUTE_SETUP_SET_LOCAL_PARAMS: &str = "/setup/set-local-params";
pub const ROUTE_SETUP_ADD_PEER: &str = "/setup/add-peer";
pub const ROUTE_SETUP_START_DKG: &str = "/setup/start-dkg";

// Dashboard routes
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

// Request/response types

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SetLocalParamsRequest {
    pub name: String,
    pub federation_name: Option<String>,
    pub federation_size: Option<u32>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SetLocalParamsResponse {
    pub setup_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddPeerRequest {
    pub setup_code: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AddPeerResponse {
    pub name: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct GatewayUrlRequest {
    pub url: String,
}
