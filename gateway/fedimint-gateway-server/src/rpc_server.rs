use std::collections::HashMap;
use std::net::SocketAddr;

use anyhow::anyhow;
use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::routing::{get, post};
use bitcoin::hashes::sha256;
use fedimint_core::config::FederationId;
use fedimint_core::util::FmtCompact;
use fedimint_gateway_common::{
    CloseChannelsWithPeerRequest, ConfigPayload, ConnectFedPayload,
    CreateInvoiceForOperatorPayload, DepositAddressPayload, DepositAddressRecheckPayload,
    GetInvoiceRequest, ListTransactionsPayload, OpenChannelRequest, PayInvoiceForOperatorPayload,
    PeginFromOnchainPayload, ROUTE_BALANCES, ROUTE_ECASH_PEGIN, ROUTE_ECASH_PEGIN_FROM_ONCHAIN,
    ROUTE_ECASH_PEGIN_RECHECK, ROUTE_ECASH_PEGOUT, ROUTE_ECASH_PEGOUT_TO_ONCHAIN,
    ROUTE_ECASH_RECEIVE, ROUTE_ECASH_SEND, ROUTE_FED_CONFIG, ROUTE_FED_INVITE, ROUTE_FED_JOIN,
    ROUTE_FED_LIST, ROUTE_FED_SET_FEES, ROUTE_INFO, ROUTE_LDK_BALANCES, ROUTE_LDK_CHANNEL_CLOSE,
    ROUTE_LDK_CHANNEL_LIST, ROUTE_LDK_CHANNEL_OPEN, ROUTE_LDK_INVOICE_CREATE,
    ROUTE_LDK_INVOICE_GET, ROUTE_LDK_INVOICE_PAY, ROUTE_LDK_ONCHAIN_RECEIVE,
    ROUTE_LDK_ONCHAIN_SEND, ROUTE_LDK_PEER_CONNECT, ROUTE_LDK_PEER_DISCONNECT, ROUTE_LDK_PEER_LIST,
    ROUTE_LDK_TRANSACTION_LIST, ROUTE_MNEMONIC, ROUTE_MODULE_MINT_RECEIVE, ROUTE_MODULE_MINT_SEND,
    ROUTE_MODULE_WALLET_RECEIVE, ROUTE_STOP, ReceiveEcashPayload, SendOnchainRequest,
    SetFeesPayload, SpendEcashPayload, V1_API_ENDPOINT, WithdrawPayload, WithdrawToOnchainPayload,
};
use fedimint_lnurl::LnurlResponse;
use fedimint_lnv2_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use fedimint_lnv2_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use fedimint_logging::LOG_GATEWAY;
use hex::ToHex;
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{info, instrument, warn};

use crate::AppState;
use crate::error::{AdminGatewayError, GatewayError, LnurlError};

/// Creates two webservers:
/// - Public: binds to `0.0.0.0:<port>` for LNv2 protocol routes
/// - Admin: binds to `127.0.0.1:<port>` for all admin/operator routes (no auth
///   needed)
pub async fn run_webserver(state: AppState) -> anyhow::Result<()> {
    let task_group = state.task_group.clone();
    let listen = state.listen;

    // Public routes: LNv2 protocol endpoints accessible by federation clients
    let public_routes = public_routes()
        .with_state(state.clone())
        .layer(CorsLayer::permissive());

    let public_api = Router::new()
        .nest(&format!("/{V1_API_ENDPOINT}"), public_routes.clone())
        .merge(public_routes);

    // Admin routes: all operator endpoints, localhost-only
    let admin_routes = admin_routes()
        .with_state(state.clone())
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

/// Public routes: LNv2 protocol endpoints called by federation clients.
fn public_routes() -> Router<AppState> {
    Router::new()
        .route(ROUTING_INFO_ENDPOINT, post(routing_info_v2))
        .route(SEND_PAYMENT_ENDPOINT, post(pay_bolt11_invoice_v2))
        .route(
            CREATE_BOLT11_INVOICE_ENDPOINT,
            post(create_bolt11_invoice_v2),
        )
        .route("/verify/{payment_hash}", get(verify_bolt11_preimage_v2_get))
}

/// Admin routes: operator endpoints, only accessible via localhost.
fn admin_routes() -> Router<AppState> {
    Router::new()
        // Top-level
        .route(ROUTE_INFO, get(info))
        .route(ROUTE_BALANCES, get(balances))
        .route(ROUTE_STOP, get(stop))
        .route(ROUTE_MNEMONIC, get(mnemonic))
        // LDK node management
        .route(ROUTE_LDK_BALANCES, get(ldk_balances))
        .route(ROUTE_LDK_CHANNEL_OPEN, post(ldk_channel_open))
        .route(ROUTE_LDK_CHANNEL_CLOSE, post(ldk_channel_close))
        .route(ROUTE_LDK_CHANNEL_LIST, get(ldk_channel_list))
        .route(ROUTE_LDK_ONCHAIN_RECEIVE, get(ldk_onchain_receive))
        .route(ROUTE_LDK_ONCHAIN_SEND, post(ldk_onchain_send))
        .route(ROUTE_LDK_INVOICE_CREATE, post(ldk_invoice_create))
        .route(ROUTE_LDK_INVOICE_PAY, post(ldk_invoice_pay))
        .route(ROUTE_LDK_INVOICE_GET, post(ldk_invoice_get))
        .route(ROUTE_LDK_PEER_CONNECT, post(ldk_peer_connect))
        .route(ROUTE_LDK_PEER_DISCONNECT, post(ldk_peer_disconnect))
        .route(ROUTE_LDK_PEER_LIST, get(ldk_peer_list))
        .route(ROUTE_LDK_TRANSACTION_LIST, post(ldk_transaction_list))
        // Ecash / peg-in / peg-out
        .route(ROUTE_ECASH_PEGIN, post(ecash_pegin))
        .route(ROUTE_ECASH_PEGIN_RECHECK, post(ecash_pegin_recheck))
        .route(
            ROUTE_ECASH_PEGIN_FROM_ONCHAIN,
            post(ecash_pegin_from_onchain),
        )
        .route(ROUTE_ECASH_PEGOUT, post(ecash_pegout))
        .route(ROUTE_ECASH_PEGOUT_TO_ONCHAIN, post(ecash_pegout_to_onchain))
        .route(ROUTE_ECASH_SEND, post(ecash_send))
        .route(ROUTE_ECASH_RECEIVE, post(ecash_receive))
        // Federation management
        .route(ROUTE_FED_JOIN, post(federation_join))
        .route(ROUTE_FED_LIST, get(federation_list))
        .route(ROUTE_FED_SET_FEES, post(federation_set_fees))
        .route(ROUTE_FED_CONFIG, post(federation_config))
        .route(ROUTE_FED_INVITE, get(federation_invite))
        // Per-federation module commands
        .route(ROUTE_MODULE_MINT_SEND, post(module_mint_send))
        .route(ROUTE_MODULE_MINT_RECEIVE, post(module_mint_receive))
        .route(ROUTE_MODULE_WALLET_RECEIVE, post(module_wallet_receive))
}

// ---------------------------------------------------------------------------
// Top-level handlers
// ---------------------------------------------------------------------------

/// Display high-level information about the Gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn info(State(state): State<AppState>) -> Result<Json<serde_json::Value>, GatewayError> {
    let info = state.handle_get_info().await?;
    Ok(Json(json!(info)))
}

/// Instructs the gateway to shut down gracefully
#[instrument(target = LOG_GATEWAY, skip_all, err)]
pub(crate) async fn stop(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let task_group = state.task_group.clone();
    state.handle_shutdown_msg(task_group).await?;
    Ok(Json(json!(())))
}

/// Returns the gateway's mnemonic words
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mnemonic(State(state): State<AppState>) -> Result<Json<serde_json::Value>, GatewayError> {
    let words = state.handle_mnemonic_msg().await?;
    Ok(Json(json!(words)))
}

/// Returns the combined ecash, lightning, and onchain balances
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn balances(State(state): State<AppState>) -> Result<Json<serde_json::Value>, GatewayError> {
    let balances = state.handle_get_balances_msg().await?;
    Ok(Json(json!(balances)))
}

// ---------------------------------------------------------------------------
// LDK node management handlers
// ---------------------------------------------------------------------------

/// Returns the ecash, lightning, and onchain balances (LDK-specific)
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_balances(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let balances = state.handle_get_balances_msg().await?;
    Ok(Json(json!(balances)))
}

/// Opens a Lightning channel to a peer
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_channel_open(
    State(state): State<AppState>,
    Json(payload): Json<OpenChannelRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let funding_txid = state.handle_open_channel_msg(payload).await?;
    Ok(Json(json!(funding_txid)))
}

/// Closes all channels with a peer
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_channel_close(
    State(state): State<AppState>,
    Json(payload): Json<CloseChannelsWithPeerRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let response = state.handle_close_channels_with_peer_msg(payload).await?;
    Ok(Json(json!(response)))
}

/// Lists all Lightning channels
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let channels = state.handle_list_channels_msg().await?;
    Ok(Json(json!(channels)))
}

/// Generates an onchain address to fund the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_receive(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let address = state.handle_get_ln_onchain_address_msg().await?;
    Ok(Json(json!(address.to_string())))
}

/// Send funds from the gateway's lightning node on-chain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_send(
    State(state): State<AppState>,
    Json(payload): Json<SendOnchainRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let txid = state.handle_send_onchain_msg(payload).await?;
    Ok(Json(json!(txid)))
}

/// Creates an invoice directly payable to the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_invoice_create(
    State(state): State<AppState>,
    Json(payload): Json<CreateInvoiceForOperatorPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = state
        .handle_create_invoice_for_operator_msg(payload)
        .await?;
    Ok(Json(json!(invoice)))
}

/// Pays an outgoing LN invoice using the gateway's own funds
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_invoice_pay(
    State(state): State<AppState>,
    Json(payload): Json<PayInvoiceForOperatorPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let preimage = state.handle_pay_invoice_for_operator_msg(payload).await?;
    Ok(Json(json!(preimage.0.encode_hex::<String>())))
}

/// Retrieves an invoice by payment hash
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_invoice_get(
    State(state): State<AppState>,
    Json(payload): Json<GetInvoiceRequest>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = state.handle_get_invoice_msg(payload).await?;
    Ok(Json(json!(invoice)))
}

/// Connects to a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_connect(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let pubkey = payload["pubkey"]
        .as_str()
        .ok_or_else(|| AdminGatewayError::Unexpected(anyhow!("Missing pubkey")))?
        .parse()
        .map_err(|e| AdminGatewayError::Unexpected(anyhow!("Invalid pubkey: {e}")))?;
    let address: ldk_node::lightning::ln::msgs::SocketAddress = payload["address"]
        .as_str()
        .ok_or_else(|| AdminGatewayError::Unexpected(anyhow!("Missing address")))?
        .parse()
        .map_err(|e| AdminGatewayError::Unexpected(anyhow!("Invalid address: {e}")))?;

    state
        .node
        .connect(pubkey, address, true)
        .map_err(|e| AdminGatewayError::Unexpected(anyhow!("Failed to connect to peer: {e}")))?;

    info!(target: LOG_GATEWAY, %pubkey, "Connected to peer");
    Ok(Json(json!(())))
}

/// Disconnects from a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_disconnect(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let pubkey = payload["pubkey"]
        .as_str()
        .ok_or_else(|| AdminGatewayError::Unexpected(anyhow!("Missing pubkey")))?
        .parse()
        .map_err(|e| AdminGatewayError::Unexpected(anyhow!("Invalid pubkey: {e}")))?;

    state.node.disconnect(pubkey).map_err(|e| {
        AdminGatewayError::Unexpected(anyhow!("Failed to disconnect from peer: {e}"))
    })?;

    info!(target: LOG_GATEWAY, %pubkey, "Disconnected from peer");
    Ok(Json(json!(())))
}

/// Lists all Lightning peers
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let peers: Vec<serde_json::Value> = state
        .node
        .list_peers()
        .into_iter()
        .map(|peer| {
            json!({
                "node_id": peer.node_id.to_string(),
                "address": peer.address.to_string(),
                "is_persisted": peer.is_persisted,
                "is_connected": peer.is_connected,
            })
        })
        .collect();

    Ok(Json(json!(peers)))
}

/// Lists LN payment transactions
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_transaction_list(
    State(state): State<AppState>,
    Json(payload): Json<ListTransactionsPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let transactions = state.handle_list_transactions_msg(payload).await?;
    Ok(Json(json!(transactions)))
}

// ---------------------------------------------------------------------------
// Federation management handlers
// ---------------------------------------------------------------------------

/// Join a new federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_join(
    State(state): State<AppState>,
    Json(payload): Json<ConnectFedPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let fed = state.handle_connect_federation(payload).await?;
    Ok(Json(json!(fed)))
}

/// List connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let dbtx = state.gateway_db.begin_transaction_nc().await;
    let federations = state
        .federation_manager
        .read()
        .await
        .federation_info_all_federations(dbtx)
        .await;
    Ok(Json(json!(federations)))
}

/// Set fees for one or all federations
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_set_fees(
    State(state): State<AppState>,
    Json(payload): Json<SetFeesPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    state.handle_set_fees_msg(payload).await?;
    Ok(Json(json!(())))
}

/// Display federation config
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_config(
    State(state): State<AppState>,
    Json(payload): Json<ConfigPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let gateway_fed_config = state
        .handle_get_federation_config(payload.federation_id)
        .await?;
    Ok(Json(json!(gateway_fed_config)))
}

/// Export invite codes for all connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_invite(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invite_codes = state.handle_export_invite_codes().await;
    Ok(Json(json!(invite_codes)))
}

// ---------------------------------------------------------------------------
// Per-federation module handlers
// ---------------------------------------------------------------------------

/// Spend ecash from a federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_send(
    State(state): State<AppState>,
    Json(payload): Json<SpendEcashPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!(state.handle_spend_ecash_msg(payload).await?)))
}

/// Receive ecash into the gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_receive(
    State(state): State<AppState>,
    Json(payload): Json<ReceiveEcashPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!(state.handle_receive_ecash_msg(payload).await?)))
}

/// Generate deposit address for a federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn module_wallet_receive(
    State(state): State<AppState>,
    Json(payload): Json<DepositAddressPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let address = state.handle_address_msg(payload).await?;
    Ok(Json(json!(address)))
}

// ---------------------------------------------------------------------------
// Ecash / peg-in / peg-out handlers
// ---------------------------------------------------------------------------

/// Generate a deposit address for pegging in to a federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegin(
    State(state): State<AppState>,
    Json(payload): Json<DepositAddressPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let address = state.handle_address_msg(payload).await?;
    Ok(Json(json!(address)))
}

/// Trigger rechecking for deposits on an address
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegin_recheck(
    State(state): State<AppState>,
    Json(payload): Json<DepositAddressRecheckPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    state.handle_recheck_address_msg(payload).await?;
    Ok(Json(json!({})))
}

/// Peg in from the gateway's onchain wallet into a federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegin_from_onchain(
    State(state): State<AppState>,
    Json(payload): Json<PeginFromOnchainPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let txid = state.handle_pegin_from_onchain_msg(payload).await?;
    Ok(Json(json!(txid)))
}

/// Withdraw from a federation to an on-chain address
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegout(
    State(state): State<AppState>,
    Json(payload): Json<WithdrawPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let response = state.handle_withdraw_msg(payload).await?;
    Ok(Json(json!(response)))
}

/// Withdraw from a federation to the gateway's onchain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegout_to_onchain(
    State(state): State<AppState>,
    Json(payload): Json<WithdrawToOnchainPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let response = state.handle_withdraw_to_onchain_msg(payload).await?;
    Ok(Json(json!(response)))
}

/// Spend ecash from a federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ecash_send(
    State(state): State<AppState>,
    Json(payload): Json<SpendEcashPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!(state.handle_spend_ecash_msg(payload).await?)))
}

/// Receive ecash into the gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ecash_receive(
    State(state): State<AppState>,
    Json(payload): Json<ReceiveEcashPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    Ok(Json(json!(state.handle_receive_ecash_msg(payload).await?)))
}

// ---------------------------------------------------------------------------
// LNv2 protocol handlers (public)
// ---------------------------------------------------------------------------

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn routing_info_v2(
    State(state): State<AppState>,
    Json(federation_id): Json<FederationId>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let routing_info = state.routing_info_v2(&federation_id).await?;
    Ok(Json(json!(routing_info)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn pay_bolt11_invoice_v2(
    State(state): State<AppState>,
    Json(payload): Json<SendPaymentPayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let payment_result = state.send_payment_v2(payload).await?;
    Ok(Json(json!(payment_result)))
}

#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn create_bolt11_invoice_v2(
    State(state): State<AppState>,
    Json(payload): Json<CreateBolt11InvoicePayload>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let invoice = state.create_bolt11_invoice_v2(payload).await?;
    Ok(Json(json!(invoice)))
}

pub(crate) async fn verify_bolt11_preimage_v2_get(
    State(state): State<AppState>,
    Path(payment_hash): Path<sha256::Hash>,
    Query(query): Query<HashMap<String, String>>,
) -> Result<Json<serde_json::Value>, GatewayError> {
    let response = state
        .verify_bolt11_preimage_v2(payment_hash, query.contains_key("wait"))
        .await
        .map_err(|e| LnurlError::internal(anyhow!(e)))?;

    Ok(Json(json!(LnurlResponse::Ok(response))))
}
