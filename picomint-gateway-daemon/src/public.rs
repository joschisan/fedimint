use anyhow::anyhow;
use axum::Router;
use axum::body::Body;
use axum::extract::{Json, Path, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use bitcoin::hashes::sha256;
use picomint_core::config::FederationId;
use picomint_core::task::TaskHandle;
use picomint_ln_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use picomint_ln_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use picomint_lnurl::LnurlResponse;
use picomint_logging::LOG_GATEWAY;
use reqwest::StatusCode;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::instrument;

use crate::AppState;
use crate::cli::CliError;

/// LNURL-compliant error response for verify endpoints.
#[derive(Debug)]
pub struct LnurlError {
    code: StatusCode,
    reason: anyhow::Error,
}

impl std::fmt::Display for LnurlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LNURL Error: {}", self.reason)
    }
}

impl std::error::Error for LnurlError {}

impl LnurlError {
    pub fn internal(reason: anyhow::Error) -> Self {
        Self {
            code: StatusCode::INTERNAL_SERVER_ERROR,
            reason,
        }
    }
}

impl IntoResponse for LnurlError {
    fn into_response(self) -> Response<Body> {
        let json = Json(serde_json::json!({
            "status": "ERROR",
            "reason": self.reason.to_string(),
        }));
        (self.code, json).into_response()
    }
}

pub async fn run_public(state: AppState, handle: TaskHandle) {
    let listener = TcpListener::bind(state.api_addr)
        .await
        .expect("Failed to bind public API server");

    let router = router()
        .with_state(state)
        .layer(CorsLayer::permissive())
        .into_make_service();

    axum::serve(listener, router)
        .with_graceful_shutdown(handle.make_shutdown_rx())
        .await
        .expect("Public webserver failed");
}

fn router() -> Router<AppState> {
    Router::new()
        .route(ROUTING_INFO_ENDPOINT, post(routing_info_v2))
        .route(SEND_PAYMENT_ENDPOINT, post(pay_bolt11_invoice_v2))
        .route(
            CREATE_BOLT11_INVOICE_ENDPOINT,
            post(create_bolt11_invoice_v2),
        )
        .route("/verify/{payment_hash}", get(verify_bolt11_preimage_v2_get))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn routing_info_v2(
    State(state): State<AppState>,
    Json(federation_id): Json<FederationId>,
) -> Result<Json<serde_json::Value>, CliError> {
    let routing_info = state.routing_info_v2(&federation_id).await?;
    Ok(Json(json!(routing_info)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn pay_bolt11_invoice_v2(
    State(state): State<AppState>,
    Json(payload): Json<SendPaymentPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let payment_result = state.send_payment_v2(payload).await?;
    Ok(Json(json!(payment_result)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn create_bolt11_invoice_v2(
    State(state): State<AppState>,
    Json(payload): Json<CreateBolt11InvoicePayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let invoice = state.create_bolt11_invoice_v2(payload).await?;
    Ok(Json(json!(invoice)))
}

async fn verify_bolt11_preimage_v2_get(
    State(state): State<AppState>,
    Path(payment_hash): Path<sha256::Hash>,
    Query(query): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, LnurlError> {
    let response = state
        .verify_bolt11_preimage_v2(payment_hash, query.contains_key("wait"))
        .await
        .map_err(|e| LnurlError::internal(anyhow!(e)))?;

    Ok(Json(json!(LnurlResponse::Ok(response))))
}
