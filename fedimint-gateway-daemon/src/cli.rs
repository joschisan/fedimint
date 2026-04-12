use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::{Json, State};
use axum::routing::post;
use bitcoin::FeeRate;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::task::TaskHandle;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow};
use fedimint_core::{Amount, BitcoinAmountOrAll};
use fedimint_gateway_common::{
    ChannelOpenResponse, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse,
    ConfigPayload, ConnectFedPayload, CreateInvoiceForOperatorPayload, DepositAddressPayload,
    DepositAddressResponse, ExportInviteCodesResponse, FederationBalanceInfo, FederationInfo,
    GatewayBalances, GatewayFedConfig, GatewayInfo, InvoiceCreateResponse, InvoicePayResponse,
    ListChannelsResponse, ListFederationsResponse, ListPeersResponse, ListTransactionsPayload,
    ListTransactionsResponse, MnemonicResponse, OnchainReceiveResponse, OnchainSendResponse,
    OpenChannelRequest, PayInvoiceForOperatorPayload, PeerConnectRequest, PeerDisconnectRequest,
    Preimage, ROUTE_FED_CONFIG, ROUTE_FED_INVITE, ROUTE_FED_JOIN, ROUTE_FED_LIST, ROUTE_INFO,
    ROUTE_LDK_BALANCES, ROUTE_LDK_CHANNEL_CLOSE, ROUTE_LDK_CHANNEL_LIST, ROUTE_LDK_CHANNEL_OPEN,
    ROUTE_LDK_INVOICE_CREATE, ROUTE_LDK_INVOICE_PAY, ROUTE_LDK_ONCHAIN_RECEIVE,
    ROUTE_LDK_ONCHAIN_SEND, ROUTE_LDK_PEER_CONNECT, ROUTE_LDK_PEER_DISCONNECT, ROUTE_LDK_PEER_LIST,
    ROUTE_LDK_TRANSACTION_LIST, ROUTE_MNEMONIC, ROUTE_MODULE_MINT_RECEIVE, ROUTE_MODULE_MINT_SEND,
    ROUTE_MODULE_WALLET_RECEIVE, ReceiveEcashPayload, ReceiveEcashResponse, SendOnchainRequest,
    SpendEcashPayload, SpendEcashResponse,
};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule;
use hex::ToHex;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeId;
use ldk_node::payment::{PaymentDirection, PaymentKind, PaymentStatus};
use lightning_invoice::{Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description};
use tokio::net::TcpListener;
use tower_http::cors::CorsLayer;
use tracing::{debug, error, info, info_span, instrument, warn};

use crate::error::CliError;
use crate::{AppState, UserChannelId, get_preimage_and_payment_hash};

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
        .route(ROUTE_LDK_TRANSACTION_LIST, post(ldk_transaction_list))
        // Federation management
        .route(ROUTE_FED_JOIN, post(federation_join))
        .route(ROUTE_FED_LIST, post(federation_list))
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
async fn info(State(state): State<AppState>) -> Result<Json<GatewayInfo>, CliError> {
    let node_status = state.node.status();

    Ok(Json(GatewayInfo {
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

/// Returns the ecash, lightning, and onchain balances (LDK-specific)
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_balances(State(state): State<AppState>) -> Result<Json<GatewayBalances>, CliError> {
    let federation_infos = state
        .federation_manager
        .read()
        .await
        .federation_info_all_federations()
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

    Ok(Json(balances))
}

/// Opens a Lightning channel to a peer
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_channel_open(
    State(state): State<AppState>,
    Json(payload): Json<OpenChannelRequest>,
) -> Result<Json<ChannelOpenResponse>, CliError> {
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
            Ok(Json(ChannelOpenResponse { txid: funding_txid }))
        }
        Err(err) => Err(CliError::internal(format!("Failed to open channel: {err}"))),
    }
}

/// Closes all channels with a peer
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_channel_close(
    State(state): State<AppState>,
    Json(payload): Json<CloseChannelsWithPeerRequest>,
) -> Result<Json<CloseChannelsWithPeerResponse>, CliError> {
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
    Ok(Json(response))
}

/// Lists all Lightning channels
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_list(
    State(state): State<AppState>,
) -> Result<Json<ListChannelsResponse>, CliError> {
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

    Ok(Json(ListChannelsResponse { channels }))
}

/// Generates an onchain address to fund the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_receive(
    State(state): State<AppState>,
) -> Result<Json<OnchainReceiveResponse>, CliError> {
    let address = state
        .node
        .onchain_payment()
        .new_address()
        .map_err(|e| CliError::internal(format!("Failed to get onchain address: {e}")))?;

    Ok(Json(OnchainReceiveResponse {
        address: address.to_string(),
    }))
}

/// Send funds from the gateway's lightning node on-chain wallet
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_send(
    State(state): State<AppState>,
    Json(payload): Json<SendOnchainRequest>,
) -> Result<Json<OnchainSendResponse>, CliError> {
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
    Ok(Json(OnchainSendResponse { txid }))
}

/// Creates an invoice directly payable to the gateway's lightning node
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn ldk_invoice_create(
    State(state): State<AppState>,
    Json(payload): Json<CreateInvoiceForOperatorPayload>,
) -> Result<Json<InvoiceCreateResponse>, CliError> {
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

    Ok(Json(InvoiceCreateResponse {
        invoice: invoice.to_string(),
    }))
}

/// Pays an outgoing LN invoice using the gateway's own funds
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_invoice_pay(
    State(state): State<AppState>,
    Json(payload): Json<PayInvoiceForOperatorPayload>,
) -> Result<Json<InvoicePayResponse>, CliError> {
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

    Ok(Json(InvoicePayResponse {
        preimage: preimage.0.encode_hex::<String>(),
    }))
}

/// Connects to a Lightning peer
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_connect(
    State(state): State<AppState>,
    Json(payload): Json<PeerConnectRequest>,
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
    Json(payload): Json<PeerDisconnectRequest>,
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
async fn ldk_peer_list(State(state): State<AppState>) -> Result<Json<ListPeersResponse>, CliError> {
    let peers = state
        .node
        .list_peers()
        .into_iter()
        .map(|peer| fedimint_gateway_common::PeerInfo {
            node_id: peer.node_id,
            address: peer.address.to_string(),
            is_connected: peer.is_connected,
        })
        .collect();

    Ok(Json(ListPeersResponse { peers }))
}

/// Lists LN payment transactions
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_transaction_list(
    State(state): State<AppState>,
    Json(payload): Json<ListTransactionsPayload>,
) -> Result<Json<ListTransactionsResponse>, CliError> {
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
    Ok(Json(response))
}

// ---------------------------------------------------------------------------
// Federation management handlers
// ---------------------------------------------------------------------------

/// Join a new federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_join(
    State(state): State<AppState>,
    Json(payload): Json<ConnectFedPayload>,
) -> Result<Json<FederationInfo>, CliError> {
    let invite_code = fedimint_core::invite_code::InviteCode::from_str(&payload.invite_code)
        .map_err(|e| CliError::bad_request(format!("Invalid federation member string {e:?}")))?;

    let federation_id = invite_code.federation_id();

    let mut federation_manager = state.federation_manager.write().await;

    if federation_manager.has_federation(federation_id) {
        return Err(CliError::bad_request(
            "Federation has already been registered",
        ));
    }

    let client = state
        .client_factory
        .join(&invite_code, Arc::new(state.clone()))
        .await?;

    AppState::check_federation_network(&client, state.network).await?;

    let federation_info = FederationInfo {
        federation_id,
        federation_name: federation_manager.federation_name(&client).await,
        balance_msat: client.get_balance_for_btc().await.unwrap_or_else(|err| {
            warn!(
                target: LOG_GATEWAY,
                err = %err.fmt_compact_anyhow(),
                %federation_id,
                "Balance not immediately available after joining."
            );
            Amount::default()
        }),
    };

    federation_manager.add_client(
        fedimint_core::util::Spanned::new(
            info_span!(target: LOG_GATEWAY, "client", %federation_id),
            async { client },
        )
        .await,
    );

    debug!(target: LOG_GATEWAY, %federation_id, "Federation connected");

    Ok(Json(federation_info))
}

/// List connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_list(
    State(state): State<AppState>,
) -> Result<Json<ListFederationsResponse>, CliError> {
    let federations = state
        .federation_manager
        .read()
        .await
        .federation_info_all_federations()
        .await;
    Ok(Json(ListFederationsResponse { federations }))
}

/// Display federation config
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn federation_config(
    State(state): State<AppState>,
    Json(payload): Json<ConfigPayload>,
) -> Result<Json<GatewayFedConfig>, CliError> {
    let federations = state
        .federation_manager
        .read()
        .await
        .get_all_federation_configs()
        .await;

    let gateway_fed_config = GatewayFedConfig { federations };
    Ok(Json(gateway_fed_config))
}

/// Export invite codes for all connected federations
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_invite(
    State(state): State<AppState>,
) -> Result<Json<ExportInviteCodesResponse>, CliError> {
    let fed_manager = state.federation_manager.read().await;
    let invite_codes = fed_manager.all_invite_codes().await;
    Ok(Json(ExportInviteCodesResponse { invite_codes }))
}

// ---------------------------------------------------------------------------
// Per-federation module handlers
// ---------------------------------------------------------------------------

/// Spend ecash from a federation
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_send(
    State(state): State<AppState>,
    Json(payload): Json<SpendEcashPayload>,
) -> Result<Json<SpendEcashResponse>, CliError> {
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
    Ok(Json(response))
}

/// Receive ecash into the gateway
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn module_mint_receive(
    State(state): State<AppState>,
    Json(payload): Json<ReceiveEcashPayload>,
) -> Result<Json<ReceiveEcashResponse>, CliError> {
    let ecash: fedimint_mintv2_client::ECash =
        base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes)
            .map_err(|e| CliError::bad_request(format!("Invalid ECash: {e}")))?;

    let federation_id = ecash
        .mint()
        .ok_or_else(|| CliError::bad_request("ECash does not contain federation id"))?;

    let client = state
        .select_client(federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

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
    Ok(Json(response))
}

/// Generate deposit address for a federation
#[instrument(target = LOG_GATEWAY, skip_all, err, fields(?payload))]
async fn module_wallet_receive(
    State(state): State<AppState>,
    Json(payload): Json<DepositAddressPayload>,
) -> Result<Json<DepositAddressResponse>, CliError> {
    let client = state
        .select_client(payload.federation_id)
        .await
        .map_err(|e| CliError::bad_request(e))?;

    let wallet_module = client
        .value()
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let address = wallet_module.receive().await;
    Ok(Json(DepositAddressResponse {
        address: address.to_string(),
    }))
}
