use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::anyhow;
use axum::extract::{Path, Query};
use axum::routing::{get, post};
use axum::{Extension, Json, Router};
use bitcoin::hashes::sha256;
use fedimint_core::config::FederationId;
use fedimint_core::task::TaskGroup;
use fedimint_core::util::FmtCompact;
use fedimint_gateway_common::{
    ADDRESS_ENDPOINT, ADDRESS_RECHECK_ENDPOINT, CLOSE_CHANNELS_WITH_PEER_ENDPOINT,
    CONFIGURATION_ENDPOINT, CREATE_BOLT11_INVOICE_FOR_OPERATOR_ENDPOINT,
    CloseChannelsWithPeerRequest, ConfigPayload, ConnectFedPayload,
    CreateInvoiceForOperatorPayload, DepositAddressPayload, DepositAddressRecheckPayload,
    GATEWAY_INFO_ENDPOINT, GET_BALANCES_ENDPOINT, GET_INVOICE_ENDPOINT,
    GET_LN_ONCHAIN_ADDRESS_ENDPOINT, GetInvoiceRequest, INVITE_CODES_ENDPOINT, JOIN_ENDPOINT,
    LIST_CHANNELS_ENDPOINT, LIST_TRANSACTIONS_ENDPOINT, ListTransactionsPayload, MNEMONIC_ENDPOINT,
    OPEN_CHANNEL_ENDPOINT, OPEN_CHANNEL_WITH_PUSH_ENDPOINT, OpenChannelRequest,
    PAY_INVOICE_FOR_OPERATOR_ENDPOINT, PEGIN_FROM_ONCHAIN_ENDPOINT, PayInvoiceForOperatorPayload,
    PeginFromOnchainPayload, RECEIVE_ECASH_ENDPOINT, ReceiveEcashPayload, SEND_ONCHAIN_ENDPOINT,
    SET_FEES_ENDPOINT, SPEND_ECASH_ENDPOINT, STOP_ENDPOINT, SendOnchainRequest, SetFeesPayload,
    SpendEcashPayload, V1_API_ENDPOINT, WITHDRAW_ENDPOINT, WITHDRAW_TO_ONCHAIN_ENDPOINT,
    WithdrawPayload, WithdrawToOnchainPayload,
};
use fedimint_lnurl::LnurlResponse;
use fedimint_lnv2_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use fedimint_lnv2_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use fedimint_logging::LOG_GATEWAY;
use hex::ToHex;
use serde::de::DeserializeOwned;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{info, instrument, warn};

use crate::Gateway;
use crate::error::{GatewayError, LnurlError};

/// Creates two webservers:
/// - Public: binds to `0.0.0.0:<port>` for LNv2 protocol routes
/// - Admin: binds to `127.0.0.1:<port>` for all admin/operator routes (no auth
///   needed)
pub async fn run_webserver(gateway: Arc<Gateway>) -> anyhow::Result<()> {
    let task_group = gateway.task_group.clone();
    let listen = gateway.listen;

    // Public routes: LNv2 protocol endpoints accessible by federation clients
    let public_routes = public_routes()
        .layer(Extension(gateway.clone()))
        .layer(CorsLayer::permissive());

    let public_api = Router::new()
        .nest(&format!("/{V1_API_ENDPOINT}"), public_routes.clone())
        .merge(public_routes);

    // Admin routes: all operator endpoints, localhost-only
    let admin_routes = admin_routes()
        .layer(Extension(gateway.clone()))
        .layer(Extension(task_group.clone()))
        .layer(CorsLayer::permissive());

    let admin_api = Router::new()
        .nest(&format!("/{V1_API_ENDPOINT}"), admin_routes.clone())
        .merge(admin_routes);

    // Public listener on 0.0.0.0
    let public_addr = SocketAddr::new("0.0.0.0".parse().expect("valid addr"), listen.port());
    let handle = task_group.make_handle();
    let shutdown_rx = handle.make_shutdown_rx();
    let public_listener = TcpListener::bind(public_addr).await?;
    let public_serve = axum::serve(public_listener, public_api.into_make_service());
    task_group.spawn("Gateway Public Webserver", |_| async {
        let graceful = public_serve.with_graceful_shutdown(async {
            shutdown_rx.await;
        });
        if let Err(err) = graceful.await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Error shutting down public webserver");
        }
    });
    info!(target: LOG_GATEWAY, %public_addr, "Started public webserver (LNv2 protocol)");

    // Admin listener on 127.0.0.1
    let admin_addr = SocketAddr::new("127.0.0.1".parse().expect("valid addr"), listen.port() + 1);
    let handle = task_group.make_handle();
    let shutdown_rx = handle.make_shutdown_rx();
    let admin_listener = TcpListener::bind(admin_addr).await?;
    let admin_serve = axum::serve(admin_listener, admin_api.into_make_service());
    task_group.spawn("Gateway Admin Webserver", |_| async {
        let graceful = admin_serve.with_graceful_shutdown(async {
            shutdown_rx.await;
        });
        if let Err(err) = graceful.await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Error shutting down admin webserver");
        }
    });
    info!(target: LOG_GATEWAY, %admin_addr, "Started admin webserver (localhost-only)");

    Ok(())
}

/// Registers a GET API handler for the HTTP server.
fn register_get_handler<F, Fut>(route: &str, func: F, router: Router) -> Router
where
    F: Fn(Extension<Arc<Gateway>>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Result<Json<serde_json::Value>, GatewayError>> + Send + 'static,
{
    router.route(route, get(func))
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

/// Public routes: LNv2 protocol endpoints called by federation clients.
fn public_routes() -> Router {
    let router = Router::new();
    let router = register_post_handler(ROUTING_INFO_ENDPOINT, routing_info_v2, router);
    let router = register_post_handler(SEND_PAYMENT_ENDPOINT, pay_bolt11_invoice_v2, router);
    let router = register_post_handler(
        CREATE_BOLT11_INVOICE_ENDPOINT,
        create_bolt11_invoice_v2,
        router,
    );
    router.route("/verify/{payment_hash}", get(verify_bolt11_preimage_v2_get))
}

/// Admin routes: operator endpoints, only accessible via localhost.
fn admin_routes() -> Router {
    let router = Router::new();
    let router = register_post_handler(RECEIVE_ECASH_ENDPOINT, receive_ecash, router);
    let router = register_post_handler(ADDRESS_ENDPOINT, address, router);
    let router = register_post_handler(WITHDRAW_ENDPOINT, withdraw, router);
    let router = register_post_handler(WITHDRAW_TO_ONCHAIN_ENDPOINT, withdraw_to_onchain, router);
    let router = register_post_handler(PEGIN_FROM_ONCHAIN_ENDPOINT, pegin_from_onchain, router);
    let router = register_post_handler(JOIN_ENDPOINT, join, router);
    let router = register_post_handler(
        CREATE_BOLT11_INVOICE_FOR_OPERATOR_ENDPOINT,
        create_invoice_for_operator,
        router,
    );
    let router = register_post_handler(
        PAY_INVOICE_FOR_OPERATOR_ENDPOINT,
        pay_invoice_operator,
        router,
    );
    let router = register_post_handler(GET_INVOICE_ENDPOINT, get_invoice, router);
    let router = register_get_handler(
        GET_LN_ONCHAIN_ADDRESS_ENDPOINT,
        get_ln_onchain_address,
        router,
    );
    let router = register_post_handler(OPEN_CHANNEL_ENDPOINT, open_channel, router);
    let router = register_post_handler(
        OPEN_CHANNEL_WITH_PUSH_ENDPOINT,
        open_channel_with_push,
        router,
    );
    let router = register_post_handler(
        CLOSE_CHANNELS_WITH_PEER_ENDPOINT,
        close_channels_with_peer,
        router,
    );
    let router = register_get_handler(LIST_CHANNELS_ENDPOINT, list_channels, router);
    let router = register_post_handler(LIST_TRANSACTIONS_ENDPOINT, list_transactions, router);
    let router = register_post_handler(SEND_ONCHAIN_ENDPOINT, send_onchain, router);
    let router = register_post_handler(ADDRESS_RECHECK_ENDPOINT, recheck_address, router);
    let router = register_get_handler(GET_BALANCES_ENDPOINT, get_balances, router);
    let router = register_post_handler(SPEND_ECASH_ENDPOINT, spend_ecash, router);
    let router = register_get_handler(MNEMONIC_ENDPOINT, mnemonic, router);
    let router = router.route(STOP_ENDPOINT, get(stop));
    let router = register_post_handler(SET_FEES_ENDPOINT, set_fees, router);
    let router = register_post_handler(CONFIGURATION_ENDPOINT, configuration, router);
    let router = register_get_handler(GATEWAY_INFO_ENDPOINT, info, router);
    register_get_handler(INVITE_CODES_ENDPOINT, invite_codes, router)
}

/// Display high-level information about the Gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn info(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let info = gateway.handle_get_info().await?;
    Ok(Json(json!(info)))
}

/// Display high-level information about the Gateway config
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn configuration(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<ConfigPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let gateway_fed_config = gateway
        .handle_get_federation_config(payload.federation_id)
        .await?;
    Ok(Json(json!(gateway_fed_config)))
}

/// Generate deposit address
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn address(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<DepositAddressPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let address = gateway.handle_address_msg(payload).await?;
    Ok(Json(json!(address)))
}

/// Pegs in funds from the gateway's onchain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn pegin_from_onchain(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<PeginFromOnchainPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let address = gateway.handle_pegin_from_onchain_msg(payload).await?;
    Ok(Json(json!(address)))
}

/// Withdraw from a gateway federation.
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn withdraw(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<WithdrawPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let txid = gateway.handle_withdraw_msg(payload).await?;
    Ok(Json(json!(txid)))
}

/// Withdraw from a gateway federation.
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn withdraw_to_onchain(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<WithdrawToOnchainPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let txid = gateway.handle_withdraw_to_onchain_msg(payload).await?;
    Ok(Json(json!(txid)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn create_invoice_for_operator(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<CreateInvoiceForOperatorPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = gateway
        .handle_create_invoice_for_operator_msg(payload)
        .await?;
    Ok(Json(json!(invoice)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn pay_invoice_operator(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<PayInvoiceForOperatorPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let preimage = gateway.handle_pay_invoice_for_operator_msg(payload).await?;
    Ok(Json(json!(preimage.0.encode_hex::<String>())))
}

/// Join a new federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn join(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<ConnectFedPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let fed = gateway.handle_connect_federation(payload).await?;
    Ok(Json(json!(fed)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn set_fees(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<SetFeesPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    gateway.handle_set_fees_msg(payload).await?;
    Ok(Json(json!(())))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn get_ln_onchain_address(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let address = gateway.handle_get_ln_onchain_address_msg().await?;
    Ok(Json(json!(address.to_string())))
}

#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn open_channel(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(mut payload): Json<OpenChannelRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    payload.push_amount_sats = 0;
    let funding_txid = gateway.handle_open_channel_msg(payload).await?;
    Ok(Json(json!(funding_txid)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn open_channel_with_push(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<OpenChannelRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let funding_txid = gateway.handle_open_channel_msg(payload).await?;
    Ok(Json(json!(funding_txid)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn close_channels_with_peer(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<CloseChannelsWithPeerRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let response = gateway.handle_close_channels_with_peer_msg(payload).await?;
    Ok(Json(json!(response)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn list_channels(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let channels = gateway.handle_list_channels_msg().await?;
    Ok(Json(json!(channels)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn send_onchain(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<SendOnchainRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let txid = gateway.handle_send_onchain_msg(payload).await?;
    Ok(Json(json!(txid)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn recheck_address(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<DepositAddressRecheckPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    gateway.handle_recheck_address_msg(payload).await?;
    Ok(Json(json!({})))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn get_balances(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let balances = gateway.handle_get_balances_msg().await?;
    Ok(Json(json!(balances)))
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
) -> Result<Json<serde_json::Value>, GatewayError> {
    let response = gateway
        .verify_bolt11_preimage_v2(payment_hash, query.contains_key("wait"))
        .await
        .map_err(|e| LnurlError::internal(anyhow!(e)))?;

    Ok(Json(json!(LnurlResponse::Ok(response))))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn spend_ecash(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<SpendEcashPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!(gateway.handle_spend_ecash_msg(payload).await?)))
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

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mnemonic(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let words = gateway.handle_mnemonic_msg().await?;
    Ok(Json(json!(words)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
pub(crate) async fn stop(
    Extension(task_group): Extension<TaskGroup>,
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    gateway.handle_shutdown_msg(task_group).await?;
    Ok(Json(json!(())))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn get_invoice(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<GetInvoiceRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = gateway.handle_get_invoice_msg(payload).await?;
    Ok(Json(json!(invoice)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn list_transactions(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(payload): Json<ListTransactionsPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let transactions = gateway.handle_list_transactions_msg(payload).await?;
    Ok(Json(json!(transactions)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn invite_codes(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invite_codes = gateway.handle_export_invite_codes().await;
    Ok(Json(json!(invite_codes)))
}
