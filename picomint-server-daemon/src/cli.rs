use std::path::Path;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use picomint_core::task::TaskHandle;
use picomint_server_cli_core::{
    CLI_SOCKET_FILENAME, ROUTE_SETUP_ADD_PEER, ROUTE_SETUP_RESTORE, ROUTE_SETUP_SET_LOCAL_PARAMS,
    ROUTE_SETUP_START_DKG, ROUTE_SETUP_STATUS, SetupAddPeerRequest, SetupAddPeerResponse,
    SetupSetLocalParamsRequest, SetupSetLocalParamsResponse, SetupStatus,
};
use tokio::net::UnixListener;

use crate::config::ServerConfig;
use crate::config::setup::SetupApi;
pub type DynSetupApi = Arc<SetupApi>;

#[derive(Clone)]
pub struct CliState {
    pub setup_api: DynSetupApi,
}

#[derive(Debug)]
pub struct CliError {
    pub code: StatusCode,
    pub error: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            code: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.to_string(),
        }
    }
}

impl IntoResponse for CliError {
    fn into_response(self) -> axum::response::Response {
        (self.code, self.error).into_response()
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(e)
    }
}

/// Setup CLI server — runs during DKG phase. Binds a Unix socket at
/// `{data_dir}/{CLI_SOCKET_FILENAME}`; a stale socket from a previous
/// (crashed) run is unlinked before we bind.
pub async fn run_cli(data_dir: &Path, state: CliState, handle: TaskHandle) {
    let socket_path = data_dir.join(CLI_SOCKET_FILENAME);
    std::fs::remove_file(&socket_path).ok();

    let listener = UnixListener::bind(&socket_path).expect("Failed to bind CLI server");

    let router = Router::new()
        .route(ROUTE_SETUP_STATUS, post(setup_status))
        .route(ROUTE_SETUP_SET_LOCAL_PARAMS, post(setup_set_local_params))
        .route(ROUTE_SETUP_ADD_PEER, post(setup_add_peer))
        .route(ROUTE_SETUP_START_DKG, post(setup_start_dkg))
        .route(ROUTE_SETUP_RESTORE, post(setup_restore))
        .with_state(state)
        .into_make_service();

    axum::serve(listener, router)
        .with_graceful_shutdown(handle.make_shutdown_rx())
        .await
        .expect("CLI admin server failed");
}

/// Build the Dashboard-phase CLI router that exposes read-only federation
/// endpoints (audit, invite) plus the LN/wallet module-admin routes.
pub fn dashboard_cli_router(api: Arc<crate::consensus::api::ConsensusApi>) -> Router {
    use axum::Json;
    use axum::routing::post;
    use picomint_server_cli_core::{
        AuditResponse, InviteResponse, LnGatewayRequest, ROUTE_AUDIT, ROUTE_CONFIG, ROUTE_INVITE,
        ROUTE_MODULE_LN_GATEWAY_ADD, ROUTE_MODULE_LN_GATEWAY_LIST, ROUTE_MODULE_LN_GATEWAY_REMOVE,
        ROUTE_MODULE_WALLET_BLOCK_COUNT, ROUTE_MODULE_WALLET_FEERATE,
        ROUTE_MODULE_WALLET_PENDING_TX_CHAIN, ROUTE_MODULE_WALLET_TOTAL_VALUE,
        ROUTE_MODULE_WALLET_TX_CHAIN, ROUTE_SESSION_COUNT, WalletBlockCountResponse,
        WalletFeerateResponse, WalletTotalValueResponse,
    };

    async fn config(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<ServerConfig>, CliError> {
        Ok(Json(api.cfg.clone()))
    }

    async fn session_count(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<u64>, CliError> {
        Ok(Json(api.session_count().await))
    }

    async fn invite(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<InviteResponse>, CliError> {
        Ok(Json(InviteResponse {
            invite_code: api.cfg.get_invite_code().to_string(),
        }))
    }

    async fn audit(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<AuditResponse>, CliError> {
        Ok(Json(AuditResponse {
            audit: api.federation_audit().await,
        }))
    }

    async fn wallet_total_value(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<WalletTotalValueResponse>, CliError> {
        Ok(Json(WalletTotalValueResponse {
            total_value_sats: api
                .server
                .wallet
                .federation_wallet_ui()
                .await
                .map(|w| w.value.to_sat()),
        }))
    }

    async fn wallet_block_count(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<WalletBlockCountResponse>, CliError> {
        Ok(Json(WalletBlockCountResponse {
            block_count: api.server.wallet.consensus_block_count_ui().await,
        }))
    }

    async fn wallet_feerate(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<WalletFeerateResponse>, CliError> {
        Ok(Json(WalletFeerateResponse {
            sats_per_vbyte: api.server.wallet.consensus_feerate_ui().await,
        }))
    }

    async fn wallet_pending_tx_chain(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<Vec<picomint_wallet_common::TxInfo>>, CliError> {
        Ok(Json(api.server.wallet.pending_tx_chain_ui().await))
    }

    async fn wallet_tx_chain(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<Vec<picomint_wallet_common::TxInfo>>, CliError> {
        Ok(Json(api.server.wallet.tx_chain_ui().await))
    }

    async fn ln_gateway_add(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
        Json(payload): Json<LnGatewayRequest>,
    ) -> Result<Json<bool>, CliError> {
        let url: picomint_core::util::SafeUrl = payload
            .url
            .parse()
            .map_err(|e| CliError::internal(format!("Invalid URL: {e}")))?;
        Ok(Json(api.server.ln.add_gateway_ui(url).await))
    }

    async fn ln_gateway_remove(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
        Json(payload): Json<LnGatewayRequest>,
    ) -> Result<Json<bool>, CliError> {
        let url: picomint_core::util::SafeUrl = payload
            .url
            .parse()
            .map_err(|e| CliError::internal(format!("Invalid URL: {e}")))?;
        Ok(Json(api.server.ln.remove_gateway_ui(url).await))
    }

    async fn ln_gateway_list(
        State(api): State<Arc<crate::consensus::api::ConsensusApi>>,
    ) -> Result<Json<Vec<picomint_core::util::SafeUrl>>, CliError> {
        Ok(Json(api.server.ln.gateways_ui().await))
    }

    Router::new()
        .route(ROUTE_INVITE, post(invite))
        .route(ROUTE_AUDIT, post(audit))
        .route(ROUTE_CONFIG, post(config))
        .route(ROUTE_SESSION_COUNT, post(session_count))
        .route(ROUTE_MODULE_WALLET_TOTAL_VALUE, post(wallet_total_value))
        .route(ROUTE_MODULE_WALLET_BLOCK_COUNT, post(wallet_block_count))
        .route(ROUTE_MODULE_WALLET_FEERATE, post(wallet_feerate))
        .route(
            ROUTE_MODULE_WALLET_PENDING_TX_CHAIN,
            post(wallet_pending_tx_chain),
        )
        .route(ROUTE_MODULE_WALLET_TX_CHAIN, post(wallet_tx_chain))
        .route(ROUTE_MODULE_LN_GATEWAY_ADD, post(ln_gateway_add))
        .route(ROUTE_MODULE_LN_GATEWAY_REMOVE, post(ln_gateway_remove))
        .route(ROUTE_MODULE_LN_GATEWAY_LIST, post(ln_gateway_list))
        .with_state(api)
}

/// Dashboard CLI server — runs during consensus phase. Binds a Unix
/// socket at `{data_dir}/{CLI_SOCKET_FILENAME}`; a stale socket from a
/// previous (crashed) run is unlinked before we bind.
pub async fn run_dashboard_cli(data_dir: &Path, router: Router, handle: TaskHandle) {
    let socket_path = data_dir.join(CLI_SOCKET_FILENAME);
    std::fs::remove_file(&socket_path).ok();

    let listener = UnixListener::bind(&socket_path).expect("Failed to bind module CLI server");

    axum::serve(listener, router.into_make_service())
        .with_graceful_shutdown(handle.make_shutdown_rx())
        .await
        .expect("Module CLI admin server failed");
}

// Setup handlers

async fn setup_status(State(state): State<CliState>) -> Result<Json<SetupStatus>, CliError> {
    let status = if state.setup_api.setup_code().await.is_some() {
        SetupStatus::SharingConnectionCodes
    } else {
        SetupStatus::AwaitingLocalParams
    };
    Ok(Json(status))
}

async fn setup_set_local_params(
    State(state): State<CliState>,
    Json(payload): Json<SetupSetLocalParamsRequest>,
) -> Result<Json<SetupSetLocalParamsResponse>, CliError> {
    let setup_code = state
        .setup_api
        .set_local_parameters(
            payload.name,
            payload.federation_name,
            payload.federation_size,
        )
        .await
        .map_err(CliError::internal)?;

    Ok(Json(SetupSetLocalParamsResponse { setup_code }))
}

async fn setup_add_peer(
    State(state): State<CliState>,
    Json(payload): Json<SetupAddPeerRequest>,
) -> Result<Json<SetupAddPeerResponse>, CliError> {
    let name = state
        .setup_api
        .add_peer_setup_code(payload.setup_code)
        .await
        .map_err(CliError::internal)?;

    Ok(Json(SetupAddPeerResponse { name }))
}

async fn setup_start_dkg(State(state): State<CliState>) -> Result<Json<()>, CliError> {
    state
        .setup_api
        .start_dkg()
        .await
        .map_err(CliError::internal)?;

    Ok(Json(()))
}

async fn setup_restore(
    State(state): State<CliState>,
    Json(cfg): Json<ServerConfig>,
) -> Result<Json<()>, CliError> {
    state
        .setup_api
        .restore_config(cfg)
        .await
        .map_err(CliError::internal)?;

    Ok(Json(()))
}
