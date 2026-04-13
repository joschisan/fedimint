use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Json, State};
use axum::response::IntoResponse;
use axum::routing::post;
use bitcoin::FeeRate;
use fedimint_core::BitcoinAmountOrAll;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::task::TaskHandle;
use fedimint_core::util::FmtCompact;
use fedimint_gateway_cli_core::{
    ChannelInfo, FederationBalanceRequest, FederationBalanceResponse, FederationConfigRequest,
    FederationConfigResponse, FederationInviteResponse, FederationJoinRequest,
    FederationListResponse, InfoResponse, LdkBalancesResponse, LdkChannelCloseRequest,
    LdkChannelCloseResponse, LdkChannelListResponse, LdkChannelOpenRequest,
    LdkInvoiceCreateRequest, LdkInvoiceCreateResponse, LdkInvoicePayRequest, LdkInvoicePayResponse,
    LdkOnchainReceiveResponse, LdkOnchainSendRequest, LdkOnchainSendResponse,
    LdkPeerConnectRequest, LdkPeerDisconnectRequest, LdkPeerListResponse, MintReceiveRequest,
    MintReceiveResponse, MintSendRequest, MintSendResponse, MnemonicResponse, PeerInfo,
    ROUTE_FEDERATION_BALANCE, ROUTE_FEDERATION_CONFIG, ROUTE_FEDERATION_INVITE,
    ROUTE_FEDERATION_JOIN, ROUTE_FEDERATION_LIST, ROUTE_INFO, ROUTE_LDK_BALANCES,
    ROUTE_LDK_CHANNEL_CLOSE, ROUTE_LDK_CHANNEL_LIST, ROUTE_LDK_CHANNEL_OPEN,
    ROUTE_LDK_INVOICE_CREATE, ROUTE_LDK_INVOICE_PAY, ROUTE_LDK_ONCHAIN_RECEIVE,
    ROUTE_LDK_ONCHAIN_SEND, ROUTE_LDK_PEER_CONNECT, ROUTE_LDK_PEER_DISCONNECT, ROUTE_LDK_PEER_LIST,
    ROUTE_MNEMONIC, ROUTE_MODULE_MINT_RECEIVE, ROUTE_MODULE_MINT_SEND, ROUTE_MODULE_WALLET_RECEIVE,
    WalletReceiveRequest, WalletReceiveResponse,
};
use fedimint_gwv2_client::Preimage;
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule;
use hex::ToHex;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeId;
use ldk_node::payment::{PaymentKind, PaymentStatus};
use lightning_invoice::{Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description};
use reqwest::StatusCode;
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, info_span, instrument};

use crate::AppState;

/// Simple error type for CLI/admin endpoints.
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
    pub fn bad_request(error: impl std::fmt::Display) -> Self {
        Self {
            code: StatusCode::BAD_REQUEST,
            error: error.to_string(),
        }
    }

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

impl From<fedimint_gwv2_client::LightningRpcError> for CliError {
    fn from(e: fedimint_gwv2_client::LightningRpcError) -> Self {
        Self::internal(e)
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(e)
    }
}

pub async fn run_cli(state: AppState, handle: TaskHandle) {
    let listener = TcpListener::bind(state.cli_bind)
        .await
        .expect("Failed to bind CLI server");

    let router = router()
        .with_state(state)
        .layer(CorsLayer::permissive())
        .into_make_service();

    axum::serve(listener, router)
        .with_graceful_shutdown(handle.make_shutdown_rx())
        .await
        .expect("CLI webserver failed");
}

fn router() -> Router<AppState> {
    Router::new()
        // Top-level
        .route(ROUTE_INFO, post(info))
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
        // Federation management
        .route(ROUTE_FEDERATION_JOIN, post(federation_join))
        .route(ROUTE_FEDERATION_LIST, post(federation_list))
        .route(ROUTE_FEDERATION_CONFIG, post(federation_config))
        .route(ROUTE_FEDERATION_INVITE, post(federation_invite))
        .route(ROUTE_FEDERATION_BALANCE, post(federation_balance))
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
async fn info(State(state): State<AppState>) -> Result<Json<InfoResponse>, CliError> {
    let node_status = state.node.status();

    Ok(Json(InfoResponse {
        public_key: state.node.node_id(),
        alias: state
            .node
            .node_alias()
            .expect("node alias is set")
            .to_string(),
        network: state.node.config().network.to_string(),
        block_height: u64::from(node_status.current_best_block.height),
        synced_to_chain: node_status.latest_lightning_wallet_sync_timestamp.is_some(),
    }))
}

/// Returns the gateway's mnemonic words
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mnemonic(State(state): State<AppState>) -> Result<Json<MnemonicResponse>, CliError> {
    let words = state
        .client_factory
        .mnemonic()
        .words()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();

    Ok(Json(MnemonicResponse { mnemonic: words }))
}

// ---------------------------------------------------------------------------
// LDK node management handlers
// ---------------------------------------------------------------------------

/// Returns the onchain and lightning channel capacity balances
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_balances(
    State(state): State<AppState>,
) -> Result<Json<LdkBalancesResponse>, CliError> {
    let node_balances = state.node.list_balances();

    let channels = state.node.list_channels();
    let total_inbound_capacity_msat: u64 = channels
        .iter()
        .filter(|chan| chan.is_usable)
        .map(|channel| channel.inbound_capacity_msat)
        .sum();
    let total_outbound_capacity_msat: u64 = channels
        .iter()
        .filter(|chan| chan.is_usable)
        .map(|channel| channel.outbound_capacity_msat)
        .sum();

    Ok(Json(LdkBalancesResponse {
        total_onchain_balance_sats: node_balances.total_onchain_balance_sats,
        total_inbound_capacity_msat,
        total_outbound_capacity_msat,
    }))
}

/// Opens a Lightning channel to a peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_open(
    State(state): State<AppState>,
    Json(payload): Json<LdkChannelOpenRequest>,
) -> Result<Json<()>, CliError> {
    let push_amount_msats = if payload.push_amount_sats == 0 {
        None
    } else {
        Some(payload.push_amount_sats * 1000)
    };

    state
        .node
        .open_announced_channel(
            payload.pubkey,
            SocketAddress::from_str(&payload.host)
                .map_err(|e| CliError::internal(format!("Invalid address: {e}")))?,
            payload.channel_size_sats,
            push_amount_msats,
            None,
        )
        .map_err(|e| CliError::internal(format!("Failed to open channel: {e}")))?;

    info!(target: LOG_GATEWAY, pubkey = %payload.pubkey, "Initiated channel open");
    Ok(Json(()))
}

/// Closes all channels with a peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_close(
    State(state): State<AppState>,
    Json(payload): Json<LdkChannelCloseRequest>,
) -> Result<Json<LdkChannelCloseResponse>, CliError> {
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

    info!(target: LOG_GATEWAY, pubkey = %payload.pubkey, "Initiated channel closure");
    let response = LdkChannelCloseResponse {
        num_channels_closed,
    };
    Ok(Json(response))
}

/// Lists all Lightning channels
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_list(
    State(state): State<AppState>,
) -> Result<Json<LdkChannelListResponse>, CliError> {
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

        channels.push(ChannelInfo {
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

    Ok(Json(LdkChannelListResponse { channels }))
}

/// Generates an onchain address to fund the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_receive(
    State(state): State<AppState>,
) -> Result<Json<LdkOnchainReceiveResponse>, CliError> {
    let address = state
        .node
        .onchain_payment()
        .new_address()
        .map_err(|e| CliError::internal(format!("Failed to get onchain address: {e}")))?;

    Ok(Json(LdkOnchainReceiveResponse {
        address: address.as_unchecked().clone(),
    }))
}

/// Send funds from the gateway's lightning node on-chain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_send(
    State(state): State<AppState>,
    Json(payload): Json<LdkOnchainSendRequest>,
) -> Result<Json<LdkOnchainSendResponse>, CliError> {
    let onchain = state.node.onchain_payment();
    let retain_reserves = false;
    let checked_address = payload.address.clone().assume_checked();
    let txid = match payload.amount {
        BitcoinAmountOrAll::All => onchain
            .send_all_to_address(
                &checked_address,
                retain_reserves,
                FeeRate::from_sat_per_vb(payload.sats_per_vbyte),
            )
            .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?,
        BitcoinAmountOrAll::Amount(amount_sats) => onchain
            .send_to_address(
                &checked_address,
                amount_sats.to_sat(),
                FeeRate::from_sat_per_vb(payload.sats_per_vbyte),
            )
            .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?,
    };
    info!(target: LOG_GATEWAY, txid = %txid, "Sent onchain transaction");
    Ok(Json(LdkOnchainSendResponse { txid }))
}

/// Creates an invoice directly payable to the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_invoice_create(
    State(state): State<AppState>,
    Json(payload): Json<LdkInvoiceCreateRequest>,
) -> Result<Json<LdkInvoiceCreateResponse>, CliError> {
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

    Ok(Json(LdkInvoiceCreateResponse {
        invoice: invoice.to_string(),
    }))
}

/// Pays an outgoing LN invoice using the gateway's own funds
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_invoice_pay(
    State(state): State<AppState>,
    Json(payload): Json<LdkInvoicePayRequest>,
) -> Result<Json<LdkInvoicePayResponse>, CliError> {
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

    Ok(Json(LdkInvoicePayResponse {
        preimage: preimage.0.encode_hex::<String>(),
    }))
}

/// Connects to a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_connect(
    State(state): State<AppState>,
    Json(payload): Json<LdkPeerConnectRequest>,
) -> Result<Json<()>, CliError> {
    let address: SocketAddress = payload
        .host
        .parse()
        .map_err(|e| CliError::bad_request(format!("Invalid address: {e}")))?;

    state
        .node
        .connect(payload.pubkey, address, true)
        .map_err(|e| CliError::internal(format!("Failed to connect to peer: {e}")))?;

    info!(target: LOG_GATEWAY, pubkey = %payload.pubkey, "Connected to peer");
    Ok(Json(()))
}

/// Disconnects from a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_disconnect(
    State(state): State<AppState>,
    Json(payload): Json<LdkPeerDisconnectRequest>,
) -> Result<Json<()>, CliError> {
    state
        .node
        .disconnect(payload.pubkey)
        .map_err(|e| CliError::internal(format!("Failed to disconnect from peer: {e}")))?;

    info!(target: LOG_GATEWAY, pubkey = %payload.pubkey, "Disconnected from peer");
    Ok(Json(()))
}

/// Lists all Lightning peers
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_list(
    State(state): State<AppState>,
) -> Result<Json<LdkPeerListResponse>, CliError> {
    let peers = state
        .node
        .list_peers()
        .into_iter()
        .map(|peer| PeerInfo {
            node_id: peer.node_id,
            address: peer.address.to_string(),
            is_connected: peer.is_connected,
        })
        .collect();

    Ok(Json(LdkPeerListResponse { peers }))
}

// ---------------------------------------------------------------------------
// Federation management handlers
// ---------------------------------------------------------------------------

/// Join a new federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_join(
    State(state): State<AppState>,
    Json(payload): Json<FederationJoinRequest>,
) -> Result<Json<()>, CliError> {
    let invite_code = fedimint_core::invite_code::InviteCode::from_str(&payload.invite)
        .map_err(|e| CliError::bad_request(format!("Invalid federation member string {e:?}")))?;

    let federation_id = invite_code.federation_id();

    if state.clients.read().await.contains_key(&federation_id) {
        return Err(CliError::bad_request(
            "Federation has already been registered",
        ));
    }

    let client = state
        .client_factory
        .join(&invite_code, Arc::new(state.clone()))
        .await?;

    AppState::check_federation_network(&client, state.network).await?;

    let spanned = fedimint_core::util::Spanned::new(
        info_span!(target: LOG_GATEWAY, "client", %federation_id),
        async { client },
    )
    .await;
    state.clients.write().await.insert(federation_id, spanned);

    debug!(target: LOG_GATEWAY, %federation_id, "Federation connected");

    Ok(Json(()))
}

/// List connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_list(
    State(state): State<AppState>,
) -> Result<Json<FederationListResponse>, CliError> {
    let federations = state.federation_info_all().await;
    Ok(Json(FederationListResponse { federations }))
}

/// Display federation config
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_config(
    State(state): State<AppState>,
    Json(_payload): Json<FederationConfigRequest>,
) -> Result<Json<FederationConfigResponse>, CliError> {
    let federations = state.all_federation_configs().await;
    Ok(Json(FederationConfigResponse { federations }))
}

/// Get a federation's ecash balance
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_balance(
    State(state): State<AppState>,
    Json(payload): Json<FederationBalanceRequest>,
) -> Result<Json<FederationBalanceResponse>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .ok_or(CliError::bad_request("Federation not connected"))?;

    let balance_msat = client
        .value()
        .get_balance()
        .await
        .map_err(|e| CliError::internal(format!("Failed to read balance: {e}")))?;

    Ok(Json(FederationBalanceResponse { balance_msat }))
}

/// Export invite codes for all connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_invite(
    State(state): State<AppState>,
) -> Result<Json<FederationInviteResponse>, CliError> {
    let invite_codes = state.all_invite_codes().await;
    Ok(Json(FederationInviteResponse { invite_codes }))
}

// ---------------------------------------------------------------------------
// Per-federation module handlers
// ---------------------------------------------------------------------------

/// Spend ecash from a federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_send(
    State(state): State<AppState>,
    Json(payload): Json<MintSendRequest>,
) -> Result<Json<MintSendResponse>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .ok_or(CliError::bad_request("Federation not connected"))?
        .into_value();

    let mint_module = client
        .get_first_module::<MintClientModule>()
        .map_err(|e| CliError::internal(e))?;
    let ecash = mint_module
        .send(payload.amount, serde_json::Value::Null)
        .await
        .map_err(|e| CliError::internal(e))?;

    let response = MintSendResponse {
        notes: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
    };
    Ok(Json(response))
}

/// Receive ecash into the gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_receive(
    State(state): State<AppState>,
    Json(payload): Json<MintReceiveRequest>,
) -> Result<Json<MintReceiveResponse>, CliError> {
    let ecash: fedimint_mintv2_client::ECash =
        base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes)
            .map_err(|e| CliError::bad_request(format!("Invalid ECash: {e}")))?;

    let federation_id = ecash
        .mint()
        .ok_or_else(|| CliError::bad_request("ECash does not contain federation id"))?;

    let client = state
        .select_client(federation_id)
        .await
        .ok_or(CliError::bad_request("Federation not connected"))?;

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

    let response = MintReceiveResponse { amount };
    Ok(Json(response))
}

/// Generate deposit address for a federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_wallet_receive(
    State(state): State<AppState>,
    Json(payload): Json<WalletReceiveRequest>,
) -> Result<Json<WalletReceiveResponse>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .ok_or(CliError::bad_request("Federation not connected"))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let address = wallet_module.receive().await;
    Ok(Json(WalletReceiveResponse {
        address: address.as_unchecked().clone(),
    }))
}
