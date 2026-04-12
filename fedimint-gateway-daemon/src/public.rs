use anyhow::anyhow;
use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::routing::{get, post};
use bitcoin::hashes::sha256;
use fedimint_core::config::FederationId;
use fedimint_gateway_common::V1_API_ENDPOINT;
use fedimint_lnurl::LnurlResponse;
use fedimint_lnv2_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use fedimint_lnv2_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use fedimint_logging::LOG_GATEWAY;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{info, instrument};

use crate::AppState;
use crate::error::{CliError, LnurlError};

pub async fn run_public(listener: TcpListener, state: AppState) {
    let routes = router()
        .with_state(state.clone())
        .layer(CorsLayer::permissive());

    let api = Router::new()
        .nest(&format!("/{V1_API_ENDPOINT}"), routes.clone())
        .merge(routes);

    let addr = listener.local_addr().expect("valid local addr");
    info!(target: LOG_GATEWAY, %addr, "Started public webserver (LNv2 protocol)");

    let handle = state.task_group.make_handle();
    axum::serve(listener, api.into_make_service())
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
