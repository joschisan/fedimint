use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::str::FromStr;
use std::time::Duration;

use anyhow::anyhow;
use axum::Router;
use axum::extract::{Json, Path, Query, State};
use axum::routing::{get, post};
use bitcoin::hashes::sha256;
use bitcoin::{Address, FeeRate};
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::config::FederationId;
use fedimint_core::module::CommonModuleInit;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow};
use fedimint_core::{Amount, BitcoinAmountOrAll, fedimint_build_code_version_env};
use fedimint_gateway_common::{
    CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse, ConfigPayload, ConnectFedPayload,
    CreateInvoiceForOperatorPayload, DepositAddressPayload, FederationBalanceInfo,
    FederationConfig, FederationInfo, GatewayBalances, GatewayFedConfig, GatewayInfo,
    LightningInfo, ListTransactionsPayload, ListTransactionsResponse, MnemonicResponse,
    OpenChannelRequest, PayInvoiceForOperatorPayload, PaymentFee, PeginFromOnchainPayload,
    Preimage, ROUTE_BALANCES, ROUTE_ECASH_PEGIN, ROUTE_ECASH_PEGIN_FROM_ONCHAIN,
    ROUTE_ECASH_PEGOUT, ROUTE_ECASH_PEGOUT_TO_ONCHAIN, ROUTE_ECASH_RECEIVE, ROUTE_ECASH_SEND,
    ROUTE_FED_CONFIG, ROUTE_FED_INVITE, ROUTE_FED_JOIN, ROUTE_FED_LIST, ROUTE_FED_SET_FEES,
    ROUTE_INFO, ROUTE_LDK_BALANCES, ROUTE_LDK_CHANNEL_CLOSE, ROUTE_LDK_CHANNEL_LIST,
    ROUTE_LDK_CHANNEL_OPEN, ROUTE_LDK_INVOICE_CREATE, ROUTE_LDK_INVOICE_PAY,
    ROUTE_LDK_ONCHAIN_RECEIVE, ROUTE_LDK_ONCHAIN_SEND, ROUTE_LDK_PEER_CONNECT,
    ROUTE_LDK_PEER_DISCONNECT, ROUTE_LDK_PEER_LIST, ROUTE_LDK_TRANSACTION_LIST, ROUTE_MNEMONIC,
    ROUTE_MODULE_MINT_RECEIVE, ROUTE_MODULE_MINT_SEND, ROUTE_MODULE_WALLET_RECEIVE, ROUTE_STOP,
    ReceiveEcashPayload, ReceiveEcashResponse, SendOnchainRequest, SetFeesPayload,
    SpendEcashPayload, SpendEcashResponse, V1_API_ENDPOINT, WithdrawPayload,
    WithdrawToOnchainPayload,
};
use fedimint_lnurl::LnurlResponse;
use fedimint_lnv2_common::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use fedimint_lnv2_common::gateway_api::{CreateBolt11InvoicePayload, SendPaymentPayload};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule;
use hex::ToHex;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeId;
use ldk_node::payment::{PaymentDirection, PaymentKind, PaymentStatus};
use lightning_invoice::{Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description};
use serde_json::json;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, info_span, instrument, warn};

use crate::db::GatewayDbtxNcExt as _;
use crate::error::{CliError, FederationNotConnected, LnurlError};
use crate::{AppState, GatewayState, UserChannelId, get_preimage_and_payment_hash, withdraw_v2};

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
        .route(ROUTE_INFO, post(info))
        .route(ROUTE_BALANCES, post(balances))
        .route(ROUTE_STOP, post(stop))
        .route(ROUTE_MNEMONIC, post(mnemonic))
        // LDK node management
        .route(ROUTE_LDK_BALANCES, post(ldk_balances))
        .route(ROUTE_LDK_CHANNEL_OPEN, post(ldk_channel_open))
        .route(ROUTE_LDK_CHANNEL_CLOSE, post(ldk_channel_close))
        .route(ROUTE_LDK_CHANNEL_LIST, post(ldk_channel_list))
        .route(ROUTE_LDK_ONCHAIN_RECEIVE, post(ldk_onchain_receive))
        .route(ROUTE_LDK_ONCHAIN_SEND, post(ldk_onchain_send))
        .route(ROUTE_LDK_INVOICE_CREATE, post(ldk_invoice_create))
        .route(ROUTE_LDK_INVOICE_PAY, post(ldk_invoice_pay))
        .route(ROUTE_LDK_PEER_CONNECT, post(ldk_peer_connect))
        .route(ROUTE_LDK_PEER_DISCONNECT, post(ldk_peer_disconnect))
        .route(ROUTE_LDK_PEER_LIST, post(ldk_peer_list))
        .route(ROUTE_LDK_TRANSACTION_LIST, post(ldk_transaction_list))
        // Ecash / peg-in / peg-out
        .route(ROUTE_ECASH_PEGIN, post(ecash_pegin))
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
        .route(ROUTE_FED_LIST, post(federation_list))
        .route(ROUTE_FED_SET_FEES, post(federation_set_fees))
        .route(ROUTE_FED_CONFIG, post(federation_config))
        .route(ROUTE_FED_INVITE, post(federation_invite))
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
async fn info(State(state): State<AppState>) -> Result<Json<serde_json::Value>, CliError> {
    let dbtx = state.gateway_db.begin_transaction_nc().await;
    let federations = state
        .federation_manager
        .read()
        .await
        .federation_info_all_federations(dbtx)
        .await;

    let channels: BTreeMap<u64, FederationId> = federations
        .iter()
        .map(|federation_info| {
            (
                federation_info.config.federation_index,
                federation_info.federation_id,
            )
        })
        .collect();

    let node_status = state.node.status();
    let synced_to_chain = node_status.latest_lightning_wallet_sync_timestamp.is_some();
    let block_height = u64::from(node_status.current_best_block.height);
    let alias = match state.node.node_alias() {
        Some(alias) => alias.to_string(),
        None => format!("LDK Fedimint Gateway Node {}", state.node.node_id()),
    };

    let lightning_info = LightningInfo::Connected {
        public_key: state.node.node_id(),
        alias,
        network: state.node.config().network.to_string(),
        block_height,
        synced_to_chain,
    };

    let info = GatewayInfo {
        federations,
        federation_fake_scids: Some(channels),
        version_hash: fedimint_build_code_version_env!().to_string(),
        gateway_state: state.state.read().await.to_string(),
        lightning_info,
        registrations: state
            .registrations
            .iter()
            .map(|(k, v)| (k.clone(), (v.endpoint_url.clone(), v.keypair.public_key())))
            .collect(),
    };

    Ok(Json(json!(info)))
}

/// Instructs the gateway to shut down gracefully
#[instrument(target = LOG_GATEWAY, skip_all, err)]
pub(crate) async fn stop(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, CliError> {
    let task_group = state.task_group.clone();

    // Take the write lock on the state so that no additional payments are processed
    let mut state_guard = state.state.write().await;
    if let GatewayState::Running = *state_guard {
        *state_guard = GatewayState::ShuttingDown;

        state
            .federation_manager
            .read()
            .await
            .wait_for_incoming_payments()
            .await?;
    }
    drop(state_guard);

    let tg = task_group.clone();
    tg.spawn("Kill Gateway", |_task_handle| async {
        if let Err(err) = task_group.shutdown_join_all(Duration::from_mins(3)).await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Error shutting down gateway");
        }
    });

    Ok(Json(json!(())))
}

/// Returns the gateway's mnemonic words
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mnemonic(State(state): State<AppState>) -> Result<Json<serde_json::Value>, CliError> {
    let mnemonic = AppState::load_mnemonic(&state.gateway_db)
        .await
        .expect("mnemonic should be set");
    let words = mnemonic
        .words()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();
    let all_federations = state
        .federation_manager
        .read()
        .await
        .get_all_federation_configs()
        .await
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    let legacy_federations = state.client_builder.legacy_federations(all_federations);
    let mnemonic_response = MnemonicResponse {
        mnemonic: words,
        legacy_federations,
    };

    Ok(Json(json!(mnemonic_response)))
}

/// Returns the combined ecash, lightning, and onchain balances
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn balances(State(state): State<AppState>) -> Result<Json<serde_json::Value>, CliError> {
    let dbtx = state.gateway_db.begin_transaction_nc().await;
    let federation_infos = state
        .federation_manager
        .read()
        .await
        .federation_info_all_federations(dbtx)
        .await;

    let ecash_balances: Vec<FederationBalanceInfo> = federation_infos
        .iter()
        .map(|federation_info| FederationBalanceInfo {
            federation_id: federation_info.federation_id,
            ecash_balance_msats: Amount {
                msats: federation_info.balance_msat.msats,
            },
        })
        .collect();

    let node_balances = state.node.list_balances();
    let total_inbound_liquidity_msats: u64 = state
        .node
        .list_channels()
        .iter()
        .filter(|chan| chan.is_usable)
        .map(|channel| channel.inbound_capacity_msat)
        .sum();

    let balances = GatewayBalances {
        onchain_balance_sats: node_balances.total_onchain_balance_sats,
        lightning_balance_msats: node_balances.total_lightning_balance_sats * 1000,
        ecash_balances,
        inbound_lightning_liquidity_msats: total_inbound_liquidity_msats,
    };

    Ok(Json(json!(balances)))
}

// ---------------------------------------------------------------------------
// LDK node management handlers
// ---------------------------------------------------------------------------

/// Returns the ecash, lightning, and onchain balances (LDK-specific)
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_balances(State(state): State<AppState>) -> Result<Json<serde_json::Value>, CliError> {
    let dbtx = state.gateway_db.begin_transaction_nc().await;
    let federation_infos = state
        .federation_manager
        .read()
        .await
        .federation_info_all_federations(dbtx)
        .await;

    let ecash_balances: Vec<FederationBalanceInfo> = federation_infos
        .iter()
        .map(|federation_info| FederationBalanceInfo {
            federation_id: federation_info.federation_id,
            ecash_balance_msats: Amount {
                msats: federation_info.balance_msat.msats,
            },
        })
        .collect();

    let node_balances = state.node.list_balances();
    let total_inbound_liquidity_msats: u64 = state
        .node
        .list_channels()
        .iter()
        .filter(|chan| chan.is_usable)
        .map(|channel| channel.inbound_capacity_msat)
        .sum();

    let balances = GatewayBalances {
        onchain_balance_sats: node_balances.total_onchain_balance_sats,
        lightning_balance_msats: node_balances.total_lightning_balance_sats * 1000,
        ecash_balances,
        inbound_lightning_liquidity_msats: total_inbound_liquidity_msats,
    };

    Ok(Json(json!(balances)))
}

/// Opens a Lightning channel to a peer
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_channel_open(
    State(state): State<AppState>,
    Json(payload): Json<OpenChannelRequest>,
) -> Result<Json<serde_json::Value>, CliError> {
    info!(
        target: LOG_GATEWAY,
        pubkey = %payload.pubkey,
        host = %payload.host,
        amount = %payload.channel_size_sats,
        "Opening Lightning channel...",
    );

    let push_amount_msats_or = if payload.push_amount_sats == 0 {
        None
    } else {
        Some(payload.push_amount_sats * 1000)
    };

    let (tx, rx) = tokio::sync::oneshot::channel::<anyhow::Result<bitcoin::OutPoint>>();

    {
        let mut channels = state.pending_channels.write().await;
        let user_channel_id = state
            .node
            .open_announced_channel(
                payload.pubkey,
                SocketAddress::from_str(&payload.host)
                    .map_err(|e| CliError::internal(format!("Failed to connect to peer: {e}")))?,
                payload.channel_size_sats,
                push_amount_msats_or,
                None,
            )
            .map_err(|e| CliError::internal(format!("Failed to open channel: {e}")))?;

        channels.insert(UserChannelId(user_channel_id), tx);
    }

    match rx
        .await
        .map_err(|err| CliError::internal(format!("Failed to open channel: {err}")))?
    {
        Ok(outpoint) => {
            let funding_txid = outpoint.txid;
            info!(target: LOG_GATEWAY, txid = %funding_txid, "Initiated channel open");
            Ok(Json(json!(funding_txid)))
        }
        Err(err) => Err(CliError::internal(format!("Failed to open channel: {err}"))),
    }
}

/// Closes all channels with a peer
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_channel_close(
    State(state): State<AppState>,
    Json(payload): Json<CloseChannelsWithPeerRequest>,
) -> Result<Json<serde_json::Value>, CliError> {
    info!(target: LOG_GATEWAY, close_channel_request = %payload, "Closing lightning channel...");

    let mut num_channels_closed = 0;
    for channel_with_peer in state
        .node
        .list_channels()
        .iter()
        .filter(|channel| channel.counterparty_node_id == payload.pubkey)
    {
        if payload.force {
            match state.node.force_close_channel(
                &channel_with_peer.user_channel_id,
                payload.pubkey,
                Some("User initiated force close".to_string()),
            ) {
                Ok(()) => num_channels_closed += 1,
                Err(err) => {
                    error!(
                        pubkey = %payload.pubkey,
                        err = %err.fmt_compact(),
                        "Could not force close channel",
                    );
                }
            }
        } else {
            match state
                .node
                .close_channel(&channel_with_peer.user_channel_id, payload.pubkey)
            {
                Ok(()) => num_channels_closed += 1,
                Err(err) => {
                    error!(
                        pubkey = %payload.pubkey,
                        err = %err.fmt_compact(),
                        "Could not close channel",
                    );
                }
            }
        }
    }

    info!(target: LOG_GATEWAY, close_channel_request = %payload, "Initiated channel closure");
    let response = CloseChannelsWithPeerResponse {
        num_channels_closed,
    };
    Ok(Json(json!(response)))
}

/// Lists all Lightning channels
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, CliError> {
    let mut channels = Vec::new();
    let network_graph = state.node.network_graph();

    let peer_addresses: std::collections::HashMap<_, _> = state
        .node
        .list_peers()
        .into_iter()
        .map(|peer| (peer.node_id, peer.address.to_string()))
        .collect();

    for channel_details in &state.node.list_channels() {
        let node_id = NodeId::from_pubkey(&channel_details.counterparty_node_id);
        let node_info = network_graph.node(&node_id);

        let remote_node_alias = node_info.as_ref().and_then(|info| {
            info.announcement_info.as_ref().and_then(|announcement| {
                let alias = announcement.alias().to_string();
                if alias.is_empty() { None } else { Some(alias) }
            })
        });

        let remote_address = peer_addresses
            .get(&channel_details.counterparty_node_id)
            .cloned();

        channels.push(fedimint_gateway_common::ChannelInfo {
            remote_pubkey: channel_details.counterparty_node_id,
            remote_alias: remote_node_alias,
            remote_address,
            channel_size_sats: channel_details.channel_value_sats,
            outbound_liquidity_sats: channel_details.outbound_capacity_msat / 1000,
            inbound_liquidity_sats: channel_details.inbound_capacity_msat / 1000,
            is_usable: channel_details.is_usable,
            is_outbound: channel_details.is_outbound,
            funding_txid: channel_details.funding_txo.map(|txo| txo.txid),
        });
    }

    Ok(Json(json!(channels)))
}

/// Generates an onchain address to fund the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_receive(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, CliError> {
    let address = state
        .node
        .onchain_payment()
        .new_address()
        .map_err(|e| CliError::internal(format!("Failed to get onchain address: {e}")))?;

    Ok(Json(json!(address.to_string())))
}

/// Send funds from the gateway's lightning node on-chain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_send(
    State(state): State<AppState>,
    Json(payload): Json<SendOnchainRequest>,
) -> Result<Json<serde_json::Value>, CliError> {
    let onchain = state.node.onchain_payment();
    let retain_reserves = false;
    let checked_address = payload.address.clone().assume_checked();
    let txid = match payload.amount {
        BitcoinAmountOrAll::All => onchain
            .send_all_to_address(
                &checked_address,
                retain_reserves,
                FeeRate::from_sat_per_vb(payload.fee_rate_sats_per_vbyte),
            )
            .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?,
        BitcoinAmountOrAll::Amount(amount_sats) => onchain
            .send_to_address(
                &checked_address,
                amount_sats.to_sat(),
                FeeRate::from_sat_per_vb(payload.fee_rate_sats_per_vbyte),
            )
            .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?,
    };
    info!(onchain_request = %payload, txid = %txid, "Sent onchain transaction");
    Ok(Json(json!(txid)))
}

/// Creates an invoice directly payable to the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_invoice_create(
    State(state): State<AppState>,
    Json(payload): Json<CreateInvoiceForOperatorPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let expiry_secs = payload.expiry_secs.unwrap_or(3600);
    let description = match payload.description {
        Some(desc) => LdkBolt11InvoiceDescription::Direct(
            Description::new(desc)
                .map_err(|_| CliError::internal("Invalid invoice description"))?,
        ),
        None => LdkBolt11InvoiceDescription::Direct(Description::empty()),
    };

    let invoice = state
        .node
        .bolt11_payment()
        .receive(payload.amount_msats, &description, expiry_secs)
        .map_err(|e| CliError::internal(format!("Failed to get invoice: {e}")))?;

    Ok(Json(json!(invoice)))
}

/// Pays an outgoing LN invoice using the gateway's own funds
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_invoice_pay(
    State(state): State<AppState>,
    Json(payload): Json<PayInvoiceForOperatorPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let payment_id = state
        .node
        .bolt11_payment()
        .send(&payload.invoice, None)
        .map_err(|e| CliError::internal(format!("LDK payment failed to initialize: {e:?}")))?;

    let preimage = loop {
        if let Some(payment_details) = state.node.payment(&payment_id) {
            match payment_details.status {
                PaymentStatus::Pending => {}
                PaymentStatus::Succeeded => {
                    if let PaymentKind::Bolt11 {
                        preimage: Some(preimage),
                        ..
                    } = payment_details.kind
                    {
                        break Preimage(preimage.0);
                    }
                }
                PaymentStatus::Failed => {
                    return Err(CliError::internal("LDK payment failed"));
                }
            }
        }
        fedimint_core::runtime::sleep(Duration::from_millis(100)).await;
    };

    Ok(Json(json!(preimage.0.encode_hex::<String>())))
}

/// Connects to a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_connect(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, CliError> {
    let pubkey = payload["pubkey"]
        .as_str()
        .ok_or_else(|| CliError::bad_request("Missing pubkey"))?
        .parse()
        .map_err(|e| CliError::bad_request(format!("Invalid pubkey: {e}")))?;
    let address: SocketAddress = payload["address"]
        .as_str()
        .ok_or_else(|| CliError::bad_request("Missing address"))?
        .parse()
        .map_err(|e| CliError::bad_request(format!("Invalid address: {e}")))?;

    state
        .node
        .connect(pubkey, address, true)
        .map_err(|e| CliError::internal(format!("Failed to connect to peer: {e}")))?;

    info!(target: LOG_GATEWAY, %pubkey, "Connected to peer");
    Ok(Json(json!(())))
}

/// Disconnects from a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_disconnect(
    State(state): State<AppState>,
    Json(payload): Json<serde_json::Value>,
) -> Result<Json<serde_json::Value>, CliError> {
    let pubkey = payload["pubkey"]
        .as_str()
        .ok_or_else(|| CliError::bad_request("Missing pubkey"))?
        .parse()
        .map_err(|e| CliError::bad_request(format!("Invalid pubkey: {e}")))?;

    state
        .node
        .disconnect(pubkey)
        .map_err(|e| CliError::internal(format!("Failed to disconnect from peer: {e}")))?;

    info!(target: LOG_GATEWAY, %pubkey, "Disconnected from peer");
    Ok(Json(json!(())))
}

/// Lists all Lightning peers
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_list(State(state): State<AppState>) -> Result<Json<serde_json::Value>, CliError> {
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
) -> Result<Json<serde_json::Value>, CliError> {
    let transactions = state
        .node
        .list_payments_with_filter(|details| {
            !matches!(details.kind, PaymentKind::Onchain { .. })
                && details.latest_update_timestamp >= payload.start_secs
                && details.latest_update_timestamp < payload.end_secs
        })
        .iter()
        .map(|details| {
            let (preimage, payment_hash, payment_kind) =
                get_preimage_and_payment_hash(&details.kind);
            let direction = match details.direction {
                PaymentDirection::Outbound => fedimint_gateway_common::PaymentDirection::Outbound,
                PaymentDirection::Inbound => fedimint_gateway_common::PaymentDirection::Inbound,
            };
            let status = match details.status {
                PaymentStatus::Failed => fedimint_gateway_common::PaymentStatus::Failed,
                PaymentStatus::Succeeded => fedimint_gateway_common::PaymentStatus::Succeeded,
                PaymentStatus::Pending => fedimint_gateway_common::PaymentStatus::Pending,
            };
            fedimint_gateway_common::PaymentDetails {
                payment_hash,
                preimage: preimage.map(|p| p.to_string()),
                payment_kind,
                amount: Amount::from_msats(
                    details
                        .amount_msat
                        .expect("amountless invoices are not supported"),
                ),
                direction,
                status,
                timestamp_secs: details.latest_update_timestamp,
            }
        })
        .collect::<Vec<_>>();
    let response = ListTransactionsResponse { transactions };
    Ok(Json(json!(response)))
}

// ---------------------------------------------------------------------------
// Federation management handlers
// ---------------------------------------------------------------------------

/// Join a new federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_join(
    State(state): State<AppState>,
    Json(payload): Json<ConnectFedPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let invite_code = fedimint_core::invite_code::InviteCode::from_str(&payload.invite_code)
        .map_err(|e| CliError::bad_request(format!("Invalid federation member string {e:?}")))?;

    let federation_id = invite_code.federation_id();

    let mut federation_manager = state.federation_manager.write().await;

    // Check if this federation has already been registered
    if federation_manager.has_federation(federation_id) {
        return Err(CliError::bad_request(
            "Federation has already been registered",
        ));
    }

    // The gateway deterministically assigns a unique identifier (u64) to each
    // federation connected.
    let federation_index = federation_manager.pop_next_index()?;

    let federation_config = FederationConfig {
        invite_code,
        federation_index,
        lightning_fee: state.default_routing_fees,
        transaction_fee: state.default_transaction_fees,
    };

    let mnemonic = AppState::load_mnemonic(&state.gateway_db)
        .await
        .expect("mnemonic should be set");
    let recover = payload.recover.unwrap_or(false);
    if recover {
        state
            .client_builder
            .recover(
                federation_config.clone(),
                std::sync::Arc::new(state.clone()),
                &mnemonic,
            )
            .await?;
    }

    let client = state
        .client_builder
        .build(
            federation_config.clone(),
            std::sync::Arc::new(state.clone()),
            &mnemonic,
        )
        .await?;

    if recover {
        client
            .wait_for_all_active_state_machines()
            .await
            .map_err(|e| CliError::internal(e))?;
    }

    // Instead of using `FederationManager::federation_info`, we manually create
    // federation info here because short channel id is not yet persisted.
    let federation_info = FederationInfo {
        federation_id,
        federation_name: federation_manager.federation_name(&client).await,
        balance_msat: client.get_balance_for_btc().await.unwrap_or_else(|err| {
            warn!(
                target: LOG_GATEWAY,
                err = %err.fmt_compact_anyhow(),
                %federation_id,
                "Balance not immediately available after joining/recovering."
            );
            Amount::default()
        }),
        config: federation_config.clone(),
    };

    AppState::check_federation_network(&client, state.network).await?;

    // no need to enter span earlier, because join has a span
    federation_manager.add_client(
        federation_index,
        fedimint_core::util::Spanned::new(
            info_span!(target: LOG_GATEWAY, "client", federation_id=%federation_id.clone()),
            async { client },
        )
        .await,
    );

    let mut dbtx = state.gateway_db.begin_transaction().await;
    dbtx.save_federation_config(&federation_config).await;
    dbtx.commit_tx().await;
    debug!(
        target: LOG_GATEWAY,
        federation_id = %federation_id,
        federation_index = %federation_index,
        "Federation connected"
    );

    Ok(Json(json!(federation_info)))
}

/// List connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_list(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, CliError> {
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
) -> Result<Json<serde_json::Value>, CliError> {
    let SetFeesPayload {
        federation_id,
        ln_base,
        ln_ppm,
        tx_base,
        tx_ppm,
    } = payload;

    let mut dbtx = state.gateway_db.begin_transaction().await;
    let mut fed_configs = if let Some(fed_id) = federation_id {
        dbtx.load_federation_configs()
            .await
            .into_iter()
            .filter(|(id, _)| *id == fed_id)
            .collect::<BTreeMap<_, _>>()
    } else {
        dbtx.load_federation_configs().await
    };

    let federation_manager = state.federation_manager.read().await;

    for (federation_id, config) in &mut fed_configs {
        let mut lightning_fee = config.lightning_fee;
        if let Some(ln_base) = ln_base {
            lightning_fee.base = ln_base;
        }

        if let Some(ln_ppm) = ln_ppm {
            lightning_fee.parts_per_million = ln_ppm;
        }

        let mut transaction_fee = config.transaction_fee;
        if let Some(tx_base) = tx_base {
            transaction_fee.base = tx_base;
        }

        if let Some(tx_ppm) = tx_ppm {
            transaction_fee.parts_per_million = tx_ppm;
        }

        let client = federation_manager
            .client(federation_id)
            .ok_or(CliError::bad_request(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            }))?;
        let client_config = client.value().config().await;
        let contains_lnv2 = client_config
            .modules
            .values()
            .any(|m| fedimint_lnv2_common::LightningCommonInit::KIND == m.kind);

        // Check if the lightning fee + transaction fee is higher than the send limit
        let send_fees = lightning_fee + transaction_fee;
        if contains_lnv2 && send_fees.gt(&PaymentFee::SEND_FEE_LIMIT) {
            return Err(CliError::bad_request(format!(
                "Total Send fees exceeded {}",
                PaymentFee::SEND_FEE_LIMIT
            )));
        }

        // Check if the transaction fee is higher than the receive limit
        if contains_lnv2 && transaction_fee.gt(&PaymentFee::RECEIVE_FEE_LIMIT) {
            return Err(CliError::bad_request(format!(
                "Transaction fees exceeded RECEIVE LIMIT {}",
                PaymentFee::RECEIVE_FEE_LIMIT
            )));
        }

        config.lightning_fee = lightning_fee;
        config.transaction_fee = transaction_fee;
        dbtx.save_federation_config(config).await;
    }

    dbtx.commit_tx().await;

    Ok(Json(json!(())))
}

/// Display federation config
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_config(
    State(state): State<AppState>,
    Json(payload): Json<ConfigPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let federations = if let Some(federation_id) = payload.federation_id {
        let mut federations = BTreeMap::new();
        federations.insert(
            federation_id,
            state
                .federation_manager
                .read()
                .await
                .get_federation_config(federation_id)
                .await?,
        );
        federations
    } else {
        state
            .federation_manager
            .read()
            .await
            .get_all_federation_configs()
            .await
    };

    let gateway_fed_config = GatewayFedConfig { federations };
    Ok(Json(json!(gateway_fed_config)))
}

/// Export invite codes for all connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_invite(
    State(state): State<AppState>,
) -> Result<Json<serde_json::Value>, CliError> {
    let fed_manager = state.federation_manager.read().await;
    let invite_codes = fed_manager.all_invite_codes().await;
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
) -> Result<Json<serde_json::Value>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?
        .into_value();

    let mint_module = client
        .get_first_module::<MintClientModule>()
        .map_err(|e| CliError::internal(e))?;
    let ecash = mint_module
        .send(payload.amount, serde_json::Value::Null)
        .await
        .map_err(|e| CliError::internal(e))?;

    let response = SpendEcashResponse {
        notes: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
    };
    Ok(Json(json!(response)))
}

/// Receive ecash into the gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_receive(
    State(state): State<AppState>,
    Json(payload): Json<ReceiveEcashPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let ecash: fedimint_mintv2_client::ECash =
        base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes)
            .map_err(|e| CliError::bad_request(format!("Invalid ECash: {e}")))?;

    let federation_id_prefix = ecash
        .mint()
        .map(|id| id.to_prefix())
        .ok_or_else(|| CliError::bad_request("ECash does not contain federation id"))?;

    let client = state
        .federation_manager
        .read()
        .await
        .get_client_for_federation_id_prefix(federation_id_prefix)
        .ok_or(CliError::bad_request(FederationNotConnected {
            federation_id_prefix,
        }))?;

    let mint = client
        .value()
        .get_first_module::<MintClientModule>()
        .map_err(|e| CliError::internal(format!("Failed to receive ecash: {e}")))?;
    let amount = ecash.amount();

    let operation_id = mint
        .receive(ecash, serde_json::Value::Null)
        .await
        .map_err(|e| CliError::internal(format!("Failed to receive ecash: {e}")))?;

    if payload.wait {
        match mint.await_final_receive_operation_state(operation_id).await {
            fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
            fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
                return Err(CliError::internal("ECash receive was rejected"));
            }
        }
    }

    let response = ReceiveEcashResponse { amount };
    Ok(Json(json!(response)))
}

/// Generate deposit address for a federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn module_wallet_receive(
    State(state): State<AppState>,
    Json(payload): Json<DepositAddressPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let address = wallet_module.receive().await;
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
) -> Result<Json<serde_json::Value>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let address = wallet_module.receive().await;
    Ok(Json(json!(address)))
}

/// Peg in from the gateway's onchain wallet into a federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegin_from_onchain(
    State(state): State<AppState>,
    Json(payload): Json<PeginFromOnchainPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    // Inline handle_address_msg: get deposit address for the federation
    let client = state
        .select_client(payload.federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let address: Address = wallet_module.receive().await;

    // Inline handle_send_onchain_msg: send from LDK onchain wallet to that address
    let send_onchain = SendOnchainRequest {
        address: address.into_unchecked(),
        amount: payload.amount,
        fee_rate_sats_per_vbyte: payload.fee_rate_sats_per_vbyte,
    };

    let onchain = state.node.onchain_payment();
    let retain_reserves = false;
    let checked_address = send_onchain.address.clone().assume_checked();
    let txid = match send_onchain.amount {
        BitcoinAmountOrAll::All => onchain
            .send_all_to_address(
                &checked_address,
                retain_reserves,
                FeeRate::from_sat_per_vb(send_onchain.fee_rate_sats_per_vbyte),
            )
            .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?,
        BitcoinAmountOrAll::Amount(amount_sats) => onchain
            .send_to_address(
                &checked_address,
                amount_sats.to_sat(),
                FeeRate::from_sat_per_vb(send_onchain.fee_rate_sats_per_vbyte),
            )
            .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?,
    };
    info!(onchain_request = %send_onchain, txid = %txid, "Sent onchain transaction");

    Ok(Json(json!(txid)))
}

/// Withdraw from a federation to an on-chain address
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegout(
    State(state): State<AppState>,
    Json(payload): Json<WithdrawPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let WithdrawPayload {
        amount,
        address,
        federation_id,
        quoted_fees: _,
    } = payload;

    let address_network = fedimint_core::get_network_for_address(&address);
    let gateway_network = state.network;
    let Ok(address) = address.require_network(gateway_network) else {
        return Err(CliError::bad_request(format!(
            "Gateway is running on network {gateway_network}, but provided withdraw address is for network {address_network}"
        )));
    };

    let client = state
        .select_client(federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let response = withdraw_v2(client.value(), &wallet_module, &address, amount).await?;
    Ok(Json(json!(response)))
}

/// Withdraw from a federation to the gateway's onchain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ecash_pegout_to_onchain(
    State(state): State<AppState>,
    Json(payload): Json<WithdrawToOnchainPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    // Inline handle_get_ln_onchain_address_msg: get LDK onchain address
    let address = state
        .node
        .onchain_payment()
        .new_address()
        .map_err(|e| CliError::internal(format!("Failed to get onchain address: {e}")))?;

    // Inline handle_withdraw_msg: withdraw from federation to that address
    let withdraw_payload = WithdrawPayload {
        address: address.into_unchecked(),
        federation_id: payload.federation_id,
        amount: payload.amount,
        quoted_fees: None,
    };

    let WithdrawPayload {
        amount,
        address,
        federation_id,
        quoted_fees: _,
    } = withdraw_payload;

    let address_network = fedimint_core::get_network_for_address(&address);
    let gateway_network = state.network;
    let Ok(address) = address.require_network(gateway_network) else {
        return Err(CliError::bad_request(format!(
            "Gateway is running on network {gateway_network}, but provided withdraw address is for network {address_network}"
        )));
    };

    let client = state
        .select_client(federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let response = withdraw_v2(client.value(), &wallet_module, &address, amount).await?;
    Ok(Json(json!(response)))
}

/// Spend ecash from a federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ecash_send(
    State(state): State<AppState>,
    Json(payload): Json<SpendEcashPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?
        .into_value();

    let mint_module = client
        .get_first_module::<MintClientModule>()
        .map_err(|e| CliError::internal(e))?;
    let ecash = mint_module
        .send(payload.amount, serde_json::Value::Null)
        .await
        .map_err(|e| CliError::internal(e))?;

    let response = SpendEcashResponse {
        notes: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
    };
    Ok(Json(json!(response)))
}

/// Receive ecash into the gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ecash_receive(
    State(state): State<AppState>,
    Json(payload): Json<ReceiveEcashPayload>,
) -> Result<Json<serde_json::Value>, CliError> {
    let ecash: fedimint_mintv2_client::ECash =
        base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes)
            .map_err(|e| CliError::bad_request(format!("Invalid ECash: {e}")))?;

    let federation_id_prefix = ecash
        .mint()
        .map(|id| id.to_prefix())
        .ok_or_else(|| CliError::bad_request("ECash does not contain federation id"))?;

    let client = state
        .federation_manager
        .read()
        .await
        .get_client_for_federation_id_prefix(federation_id_prefix)
        .ok_or(CliError::bad_request(FederationNotConnected {
            federation_id_prefix,
        }))?;

    let mint = client
        .value()
        .get_first_module::<MintClientModule>()
        .map_err(|e| CliError::internal(format!("Failed to receive ecash: {e}")))?;
    let amount = ecash.amount();

    let operation_id = mint
        .receive(ecash, serde_json::Value::Null)
        .await
        .map_err(|e| CliError::internal(format!("Failed to receive ecash: {e}")))?;

    if payload.wait {
        match mint.await_final_receive_operation_state(operation_id).await {
            fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
            fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
                return Err(CliError::internal("ECash receive was rejected"));
            }
        }
    }

    let response = ReceiveEcashResponse { amount };
    Ok(Json(json!(response)))
}

// ---------------------------------------------------------------------------
// LNv2 protocol handlers (public)
// ---------------------------------------------------------------------------

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

pub(crate) async fn verify_bolt11_preimage_v2_get(
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
