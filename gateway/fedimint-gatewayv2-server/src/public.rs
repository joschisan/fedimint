//! The gateway's public API webserver.
//!
//! Serves only the public routes used by fedimint clients (the LNv2 payment
//! protocol and ecash receive); gateway administration is handled out-of-band
//! by the `gatewaydv2-cli` admin CLI over a Unix socket (see [`crate::cli`]).

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use axum::body::Body;
use axum::extract::{Path, Query};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use bitcoin::hashes::sha256;
use fedimint_core::config::FederationId;
use fedimint_lnurl::LnurlResponse;
use fedimint_lnv2_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use fedimint_lnv2_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use fedimint_logging::LOG_GATEWAY;
use reqwest::StatusCode;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{info, instrument};

use crate::{AppState, GatewayError};

/// LNURL-compliant error envelope for the verify endpoint. This is the LNURL
/// wire format (`{"status":"ERROR","reason":...}`), not error redaction, so it
/// is kept distinct from [`GatewayError`].
#[derive(Debug)]
struct LnurlError {
    code: StatusCode,
    reason: anyhow::Error,
}

impl LnurlError {
    fn internal(reason: anyhow::Error) -> Self {
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

/// Runs the public API webserver until the task is aborted (on process
/// shutdown). Spawned as a fire-and-forget task from `main`, picomint-style.
///
/// Routes are mounted unversioned only. The lnv2 client joins its absolute
/// route paths (`/routing_info`, ...) onto the gateway URL, which replaces any
/// base path, and the LUD-21 verify URL is joined relative to the bare
/// gateway URL — so v1 gatewayd's legacy `/v1` nest was unreachable here.
pub async fn run_public(state: AppState) -> anyhow::Result<()> {
    let api_addr = state.api_addr;
    let router = routes(Arc::new(state));

    let listener = TcpListener::bind(&api_addr).await?;
    info!(target: LOG_GATEWAY, %api_addr, "Successfully started webserver");
    axum::serve(listener, router.into_make_service()).await?;

    Ok(())
}

/// Gateway webserver routes. All routes are un-authenticated and used by
/// fedimint clients; there is no HTTP administration surface.
fn routes(state: Arc<AppState>) -> Router {
    Router::new()
        .route(ROUTING_INFO_ENDPOINT, post(routing_info_v2))
        .route(SEND_PAYMENT_ENDPOINT, post(pay_bolt11_invoice_v2))
        .route(
            CREATE_BOLT11_INVOICE_ENDPOINT,
            post(create_bolt11_invoice_v2),
        )
        // Verify endpoint does not have the same signature, it is handled separately
        .route("/verify/{payment_hash}", get(verify_bolt11_preimage_v2_get))
        .layer(Extension(state))
        .layer(CorsLayer::permissive())
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn routing_info_v2(
    Extension(state): Extension<Arc<AppState>>,
    Json(federation_id): Json<FederationId>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let routing_info = state.gateway_info(&federation_id).await?;
    Ok(Json(json!(routing_info)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn pay_bolt11_invoice_v2(
    Extension(state): Extension<Arc<AppState>>,
    Json(payload): Json<SendPaymentPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let payment_result = state.send(payload).await?;
    Ok(Json(json!(payment_result)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn create_bolt11_invoice_v2(
    Extension(state): Extension<Arc<AppState>>,
    Json(payload): Json<CreateBolt11InvoicePayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = state.receive(payload).await?;
    Ok(Json(json!(invoice)))
}

async fn verify_bolt11_preimage_v2_get(
    Extension(state): Extension<Arc<AppState>>,
    Path(payment_hash): Path<sha256::Hash>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, LnurlError> {
    let response = state
        .verify(payment_hash, query.contains_key("wait"))
        .await
        .map_err(|e| LnurlError::internal(anyhow!(e)))?;

    Ok(Json(json!(LnurlResponse::Ok(response))))
}
