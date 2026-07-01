use std::collections::HashMap;
use std::sync::Arc;

use anyhow::anyhow;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use bitcoin::hashes::sha256;
use fedimint_core::config::FederationId;
use fedimint_gateway_common::{RECEIVE_ECASH_ENDPOINT, ReceiveEcashPayload, V1_API_ENDPOINT};
use fedimint_lnurl::LnurlResponse;
use fedimint_lnv2_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use fedimint_lnv2_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use fedimint_logging::LOG_GATEWAY;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{info, instrument};

use crate::Gateway;
use crate::error::{GatewayError, LnurlError};

/// Runs the public API webserver until the task is aborted (on process
/// shutdown). Serves only the public routes used by fedimint clients (the LNv2
/// payment protocol and ecash receive); gateway administration is handled
/// out-of-band by the `gatewaydv2-cli` admin CLI over a Unix socket (see
/// [`crate::cli_server`]). Spawned as a fire-and-forget task from `main`,
/// picomint-style.
pub async fn run_public(gateway: Gateway) -> anyhow::Result<()> {
    let listen = gateway.listen;
    let routes = routes(Arc::new(gateway));
    let api_v1 = Router::new()
        .nest(&format!("/{V1_API_ENDPOINT}"), routes.clone())
        // Backwards compatibility: Continue supporting gateway APIs without versioning
        .merge(routes);

    let listener = TcpListener::bind(&listen).await?;
    info!(target: LOG_GATEWAY, %listen, "Successfully started webserver");
    axum::serve(listener, api_v1.into_make_service()).await?;

    Ok(())
}

/// Registers a POST API handler for the HTTP server.
fn register_post_handler<P, F, Fut>(route: &str, func: F, router: Router) -> Router
where
    P: DeserializeOwned + Send + 'static,
    F: Fn(Extension<Arc<Gateway>>, Json<P>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Json<serde_json::Value>, GatewayError>> + Send + 'static,
{
    router.route(route, post(func))
}

/// Public routes that are used in the LNv2 protocol
fn lnv2_routes() -> Router {
    let router = Router::new();
    let router = register_post_handler(ROUTING_INFO_ENDPOINT, routing_info_v2, router);
    let router = register_post_handler(SEND_PAYMENT_ENDPOINT, pay_bolt11_invoice_v2, router);
    let router = register_post_handler(
        CREATE_BOLT11_INVOICE_ENDPOINT,
        create_bolt11_invoice_v2,
        router,
    );
    // Verify endpoint does not have the same signature, it is handled separately
    router.route("/verify/{payment_hash}", get(verify_bolt11_preimage_v2_get))
}

/// Gateway Webserver routes. All routes are un-authenticated and used by
/// fedimint clients; there is no HTTP administration surface.
fn routes(gateway: Arc<Gateway>) -> Router {
    let public_routes = register_post_handler(RECEIVE_ECASH_ENDPOINT, receive_ecash, Router::new())
        .merge(lnv2_routes());

    Router::new()
        .merge(public_routes)
        .layer(Extension(gateway))
        .layer(CorsLayer::permissive())
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn routing_info_v2(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(federation_id): Json<FederationId>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let routing_info = gateway.routing_info_v2(&federation_id).await?;
    Ok(Json(json!(routing_info)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn pay_bolt11_invoice_v2(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<SendPaymentPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let payment_result = gateway.send_payment_v2(payload).await?;
    Ok(Json(json!(payment_result)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn create_bolt11_invoice_v2(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<CreateBolt11InvoicePayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = gateway.create_bolt11_invoice_v2(payload).await?;
    Ok(Json(json!(invoice)))
}

pub(crate) async fn verify_bolt11_preimage_v2_get(
    Extension(gateway): Extension<Arc<Gateway>>,
    Path(payment_hash): Path<sha256::Hash>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, LnurlError> {
    let response = gateway
        .verify_bolt11_preimage_v2(payment_hash, query.contains_key("wait"))
        .await
        .map_err(|e| LnurlError::internal(anyhow!(e)))?;

    Ok(Json(json!(LnurlResponse::Ok(response))))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn receive_ecash(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<ReceiveEcashPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!(
        gateway.handle_receive_ecash_msg(payload).await?
    )))
}
