use std::net::SocketAddr;

use axum::Router;
use axum::extract::{Json, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use fedimint_core::module::ApiAuth;
use fedimint_core::task::TaskHandle;
use fedimint_server_cli_core::{
    AddPeerRequest, AddPeerResponse, ROUTE_SETUP_ADD_PEER, ROUTE_SETUP_SET_LOCAL_PARAMS,
    ROUTE_SETUP_START_DKG, ROUTE_SETUP_STATUS, SetLocalParamsRequest, SetLocalParamsResponse,
    SetupStatus,
};
use fedimint_server_core::setup_ui::DynSetupApi;
use tokio::net::TcpListener;
use tracing::info;

use crate::LOG_CONSENSUS;

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

pub async fn run_cli(addr: SocketAddr, state: CliState, handle: TaskHandle) {
    let listener = TcpListener::bind(addr)
        .await
        .expect("Failed to bind CLI server");

    info!(target: LOG_CONSENSUS, %addr, "Started CLI admin server (localhost-only)");

    let router = Router::new()
        .route(ROUTE_SETUP_STATUS, post(setup_status))
        .route(ROUTE_SETUP_SET_LOCAL_PARAMS, post(setup_set_local_params))
        .route(ROUTE_SETUP_ADD_PEER, post(setup_add_peer))
        .route(ROUTE_SETUP_START_DKG, post(setup_start_dkg))
        .with_state(state)
        .into_make_service();

    axum::serve(listener, router)
        .with_graceful_shutdown(handle.make_shutdown_rx())
        .await
        .expect("CLI admin server failed");
}

async fn setup_status(
    State(state): State<CliState>,
) -> Result<Json<SetupStatus>, CliError> {
    let status = if state.setup_api.setup_code().await.is_some() {
        SetupStatus::SharingConnectionCodes
    } else {
        SetupStatus::AwaitingLocalParams
    };
    Ok(Json(status))
}

async fn setup_set_local_params(
    State(state): State<CliState>,
    Json(payload): Json<SetLocalParamsRequest>,
) -> Result<Json<SetLocalParamsResponse>, CliError> {
    let setup_code = state
        .setup_api
        .set_local_parameters(
            ApiAuth::new(payload.password),
            payload.name,
            payload.federation_name,
            None,
            None,
            payload.federation_size,
        )
        .await
        .map_err(CliError::internal)?;

    Ok(Json(SetLocalParamsResponse { setup_code }))
}

async fn setup_add_peer(
    State(state): State<CliState>,
    Json(payload): Json<AddPeerRequest>,
) -> Result<Json<AddPeerResponse>, CliError> {
    let name = state
        .setup_api
        .add_peer_setup_code(payload.setup_code)
        .await
        .map_err(CliError::internal)?;

    Ok(Json(AddPeerResponse { name }))
}

async fn setup_start_dkg(State(state): State<CliState>) -> Result<Json<()>, CliError> {
    state
        .setup_api
        .start_dkg()
        .await
        .map_err(CliError::internal)?;

    Ok(Json(()))
}
