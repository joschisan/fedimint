//! Admin server for `gatewaydv2`, served over a local Unix socket at
//! `{data_dir}/cli.sock` and driven by the `gatewaydv2-cli` binary.
//!
//! This is local-only (filesystem-permission gated), so unlike the HTTP API it
//! uses no bearer-token auth. Routes and payloads come from the
//! `fedimint-gatewayv2-cli-core` contract; picomint-style, each handler owns
//! its logic and operates on the LDK node / database / federation clients
//! directly, returning the matching cli-core response, which the CLI
//! pretty-prints.

use std::collections::HashMap;
use std::str::FromStr as _;

use anyhow::{Context as _, anyhow};
use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use bitcoin::FeeRate;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;
use fedimint_core::{Amount, get_network_for_address};
use fedimint_gatewayv2_cli_core as cli_core;
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule as MintV2ClientModule;
use hex::ToHex as _;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeId;
use lightning_invoice::{Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description};
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tracing::{error, info};

use crate::db::{ClientConfigKey, DisabledFederationKey};
use crate::{AppState, GatewayError, client};

/// Runs the admin server over a local Unix socket until the task is aborted (on
/// process shutdown). Mirrors [`crate::public::run_public`] but over a
/// `UnixListener`. Spawned as a fire-and-forget task from `main`.
pub async fn run_cli(state: AppState) -> anyhow::Result<()> {
    let socket_path = state.data_dir.join(cli_core::CLI_SOCKET_FILENAME);
    // Remove any stale socket left by a previous run before binding.
    let _ = std::fs::remove_file(&socket_path);

    let router = router().with_state(state);

    let listener = UnixListener::bind(&socket_path)?;
    info!(target: LOG_GATEWAY, socket = %socket_path.display(), "Started gatewaydv2 cli server");
    axum::serve(listener, router.into_make_service()).await?;

    Ok(())
}

fn router() -> Router<AppState> {
    Router::new()
        .route(cli_core::ROUTE_INFO, post(info))
        .route(cli_core::ROUTE_MNEMONIC, post(mnemonic))
        .route(cli_core::ROUTE_LDK_BALANCES, post(ldk_balances))
        .route(
            cli_core::ROUTE_LDK_ONCHAIN_RECEIVE,
            post(ldk_onchain_receive),
        )
        .route(cli_core::ROUTE_LDK_ONCHAIN_SEND, post(ldk_onchain_send))
        .route(cli_core::ROUTE_LDK_CHANNEL_OPEN, post(ldk_channel_open))
        .route(cli_core::ROUTE_LDK_CHANNEL_CLOSE, post(ldk_channel_close))
        .route(cli_core::ROUTE_LDK_CHANNEL_LIST, post(ldk_channel_list))
        .route(cli_core::ROUTE_LDK_LN_RECEIVE, post(ldk_ln_receive))
        .route(cli_core::ROUTE_LDK_LN_SEND, post(ldk_ln_send))
        .route(cli_core::ROUTE_LDK_PEER_CONNECT, post(ldk_peer_connect))
        .route(
            cli_core::ROUTE_LDK_PEER_DISCONNECT,
            post(ldk_peer_disconnect),
        )
        .route(cli_core::ROUTE_LDK_PEER_LIST, post(ldk_peer_list))
        .route(cli_core::ROUTE_FEDERATION_JOIN, post(federation_join))
        .route(cli_core::ROUTE_FEDERATION_DISABLE, post(federation_disable))
        .route(cli_core::ROUTE_FEDERATION_ENABLE, post(federation_enable))
        .route(cli_core::ROUTE_FEDERATION_LIST, post(federation_list))
        .route(cli_core::ROUTE_FEDERATION_CONFIG, post(federation_config))
        .route(cli_core::ROUTE_FEDERATION_BALANCE, post(federation_balance))
        .route(
            cli_core::ROUTE_FEDERATION_MODULE_MINT_COUNT,
            post(mint_count),
        )
        .route(cli_core::ROUTE_FEDERATION_MODULE_MINT_SEND, post(mint_send))
        .route(
            cli_core::ROUTE_FEDERATION_MODULE_MINT_RECEIVE,
            post(mint_receive),
        )
        .route(
            cli_core::ROUTE_FEDERATION_MODULE_WALLET_SEND_FEE,
            post(wallet_send_fee),
        )
        .route(
            cli_core::ROUTE_FEDERATION_MODULE_WALLET_SEND,
            post(wallet_send),
        )
        .route(
            cli_core::ROUTE_FEDERATION_MODULE_WALLET_RECEIVE,
            post(wallet_receive),
        )
}

// --- top-level ---

/// Display high-level information about the gateway.
async fn info(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let node_status = state.node.status();

    Ok(Json(json!(cli_core::InfoResponse {
        lightning_pk: state.node.node_id(),
        network: state.network.to_string(),
        block_height: u64::from(node_status.current_best_block.height),
        synced_to_chain: node_status.latest_lightning_wallet_sync_timestamp.is_some(),
    })))
}

/// Returns the gateway's mnemonic words.
async fn mnemonic(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let mnemonic = client::load_mnemonic(&state.gateway_db)
        .await
        .expect("mnemonic should be set");

    let words = mnemonic
        .words()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();

    Ok(Json(json!(cli_core::MnemonicResponse { mnemonic: words })))
}

// --- ldk node management ---

/// Returns the onchain and lightning channel capacity balances.
async fn ldk_balances(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let balances = state.node.list_balances();
    let channels = state.node.list_channels();

    let total_inbound_capacity_msat: u64 = channels
        .iter()
        .filter(|channel| channel.is_usable)
        .map(|channel| channel.inbound_capacity_msat)
        .sum();

    // Summed via whole sats, matching the granularity the channel list reports.
    let total_outbound_capacity_msat: u64 = channels
        .iter()
        .map(|channel| (channel.outbound_capacity_msat / 1000) * 1000)
        .sum();

    Ok(Json(json!(cli_core::LdkBalancesResponse {
        total_onchain_balance_sat: balances.total_onchain_balance_sats,
        total_inbound_capacity_msat,
        total_outbound_capacity_msat,
    })))
}

/// Generates an onchain address to fund the gateway's lightning node.
async fn ldk_onchain_receive(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let address = state
        .node
        .onchain_payment()
        .new_address()
        .map_err(|e| anyhow!("Failed to get onchain address: {e}"))?;

    Ok(Json(json!(cli_core::LdkOnchainReceiveResponse {
        address: address.as_unchecked().clone(),
    })))
}

/// Send funds from the gateway's lightning node on-chain wallet.
async fn ldk_onchain_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkOnchainSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    let txid = state
        .node
        .onchain_payment()
        .send_to_address(
            &req.address.assume_checked(),
            req.amount.to_sat(),
            FeeRate::from_sat_per_vb(req.sat_per_vbyte),
        )
        .map_err(|e| anyhow!("Withdraw error: {e}"))?;

    info!(target: LOG_GATEWAY, txid = %txid, "Sent onchain transaction");

    Ok(Json(json!(cli_core::LdkOnchainSendResponse { txid })))
}

/// Opens a Lightning channel to a peer. Fire-and-forget, picomint-style: the
/// funding transaction is negotiated and broadcast asynchronously; callers
/// observe it via the channel list.
async fn ldk_channel_open(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkChannelOpenRequest>,
) -> Result<Json<Value>, GatewayError> {
    let push_amount_msat = if req.push_amount_sat == 0 {
        None
    } else {
        Some(req.push_amount_sat * 1000)
    };

    state
        .node
        .open_announced_channel(
            req.pubkey,
            SocketAddress::from_str(&req.host).map_err(|e| anyhow!("Invalid address: {e}"))?,
            req.channel_size_sat,
            push_amount_msat,
            None,
        )
        .map_err(|e| anyhow!("Failed to open channel: {e}"))?;

    info!(target: LOG_GATEWAY, pubkey = %req.pubkey, "Initiated channel open");

    Ok(Json(json!(())))
}

/// Closes all channels with a peer.
async fn ldk_channel_close(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkChannelCloseRequest>,
) -> Result<Json<Value>, GatewayError> {
    info!(target: LOG_GATEWAY, pubkey = %req.pubkey, "Closing all channels with peer");

    let mut num_channels_closed = 0;

    for channel in state
        .node
        .list_channels()
        .iter()
        .filter(|channel| channel.counterparty_node_id == req.pubkey)
    {
        if req.force {
            match state.node.force_close_channel(
                &channel.user_channel_id,
                req.pubkey,
                Some("User initiated force close".to_string()),
            ) {
                Ok(()) => num_channels_closed += 1,
                Err(err) => {
                    error!(target: LOG_GATEWAY, pubkey = %req.pubkey, err = %err, "Could not force close channel");
                }
            }
        } else {
            match state
                .node
                .close_channel(&channel.user_channel_id, req.pubkey)
            {
                Ok(()) => num_channels_closed += 1,
                Err(err) => {
                    error!(target: LOG_GATEWAY, pubkey = %req.pubkey, err = %err, "Could not close channel");
                }
            }
        }
    }

    info!(target: LOG_GATEWAY, pubkey = %req.pubkey, "Initiated channel closure");

    Ok(Json(json!(cli_core::LdkChannelCloseResponse {
        num_channels_closed,
    })))
}

/// Lists all Lightning channels.
async fn ldk_channel_list(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let network_graph = state.node.network_graph();

    let peer_addresses: HashMap<_, _> = state
        .node
        .list_peers()
        .into_iter()
        .map(|peer| (peer.node_id, peer.address.to_string()))
        .collect();

    let mut channels = Vec::new();

    for channel_details in &state.node.list_channels() {
        let node_id = NodeId::from_pubkey(&channel_details.counterparty_node_id);
        let node_info = network_graph.node(&node_id);

        let remote_alias = node_info.as_ref().and_then(|info| {
            info.announcement_info.as_ref().and_then(|announcement| {
                let alias = announcement.alias().to_string();
                if alias.is_empty() { None } else { Some(alias) }
            })
        });

        let remote_address = peer_addresses
            .get(&channel_details.counterparty_node_id)
            .cloned();

        channels.push(cli_core::ChannelInfo {
            remote_pubkey: channel_details.counterparty_node_id,
            remote_alias,
            remote_address,
            channel_size_sat: channel_details.channel_value_sats,
            outbound_liquidity_sat: channel_details.outbound_capacity_msat / 1000,
            inbound_liquidity_sat: channel_details.inbound_capacity_msat / 1000,
            is_usable: channel_details.is_usable,
            is_outbound: channel_details.is_outbound,
            funding_txid: channel_details.funding_txo.map(|txo| txo.txid),
        });
    }

    Ok(Json(json!(cli_core::LdkChannelListResponse { channels })))
}

/// Creates an invoice directly payable to the gateway's lightning node.
async fn ldk_ln_receive(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkLnReceiveRequest>,
) -> Result<Json<Value>, GatewayError> {
    let expiry_secs = req.expiry_secs.unwrap_or(3600);

    let description = match req.description {
        Some(description) => LdkBolt11InvoiceDescription::Direct(
            Description::new(description).map_err(|_| anyhow!("Invalid invoice description"))?,
        ),
        None => LdkBolt11InvoiceDescription::Direct(Description::empty()),
    };

    let invoice = state
        .node
        .bolt11_payment()
        .receive(req.amount_msat, &description, expiry_secs)
        .map_err(|e| anyhow!("Failed to get invoice: {e}"))?;

    Ok(Json(json!(cli_core::LdkLnReceiveResponse {
        invoice: invoice.to_string(),
    })))
}

/// Pays an outgoing LN invoice using the gateway's own funds.
async fn ldk_ln_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkLnSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    // Those are the ldk defaults
    const BASE_FEE: u64 = 50;
    const FEE_DENOMINATOR: u64 = 100;
    const MAX_DELAY: u64 = 1008;

    let max_fee = BASE_FEE
        + req
            .invoice
            .amount_milli_satoshis()
            .context("Invoice is missing amount")?
            .saturating_div(FEE_DENOMINATOR);

    let preimage = state
        .pay(&req.invoice, MAX_DELAY, Amount::from_msats(max_fee))
        .await?;

    Ok(Json(json!(cli_core::LdkLnSendResponse {
        preimage: preimage.encode_hex::<String>(),
    })))
}

/// Connects to a Lightning peer, persisting the connection so the node
/// reconnects on restart.
async fn ldk_peer_connect(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkPeerConnectRequest>,
) -> Result<Json<Value>, GatewayError> {
    let address =
        SocketAddress::from_str(&req.host).map_err(|e| anyhow!("Invalid address: {e}"))?;

    state
        .node
        .connect(req.pubkey, address, true)
        .map_err(|e| anyhow!("Failed to connect to peer: {e}"))?;

    Ok(Json(json!(())))
}

/// Disconnects from a Lightning peer.
async fn ldk_peer_disconnect(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkPeerDisconnectRequest>,
) -> Result<Json<Value>, GatewayError> {
    state
        .node
        .disconnect(req.pubkey)
        .map_err(|e| anyhow!("Failed to disconnect from peer: {e}"))?;

    Ok(Json(json!(())))
}

/// Lists all Lightning peers.
async fn ldk_peer_list(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let peers = state
        .node
        .list_peers()
        .into_iter()
        .map(|peer| cli_core::PeerInfo {
            node_id: peer.node_id,
            address: peer.address.to_string(),
            is_connected: peer.is_connected,
        })
        .collect::<Vec<_>>();

    Ok(Json(json!(cli_core::LdkPeerListResponse { peers })))
}

// --- federation management ---

/// Join a new federation: download and persist its config; the client itself
/// is built lazily on first use.
async fn federation_join(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationJoinRequest>,
) -> Result<Json<Value>, GatewayError> {
    state.connect_federation(req.invite).await?;

    Ok(Json(json!(())))
}

/// Disable a federation's public client API. Its config and client state are
/// retained so in-flight payments settle and it can be re-enabled by joining
/// again.
async fn federation_disable(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationDisableRequest>,
) -> Result<Json<Value>, GatewayError> {
    let mut dbtx = state.gateway_db.begin_transaction().await;

    dbtx.get_value(&ClientConfigKey(req.federation_id))
        .await
        .ok_or_else(|| {
            anyhow!(
                "No federation available for prefix {}",
                req.federation_id.to_prefix()
            )
        })?;

    dbtx.insert_entry(&DisabledFederationKey(req.federation_id), &())
        .await;

    dbtx.commit_tx().await;

    Ok(Json(json!(())))
}

/// Re-enable a previously disabled federation. Blind remove — no-op if the
/// row isn't there.
async fn federation_enable(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationEnableRequest>,
) -> Result<Json<Value>, GatewayError> {
    let mut dbtx = state.gateway_db.begin_transaction().await;
    dbtx.remove_entry(&DisabledFederationKey(req.federation_id))
        .await;
    dbtx.commit_tx().await;

    Ok(Json(json!(())))
}

/// List connected federations.
async fn federation_list(State(state): State<AppState>) -> Result<Json<Value>, GatewayError> {
    let federations = state.federation_list().await;

    Ok(Json(json!(cli_core::FederationListResponse {
        federations
    })))
}

/// Display federation config.
async fn federation_config(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationConfigRequest>,
) -> Result<Json<Value>, GatewayError> {
    let client = state.select_client(req.federation_id).await?;

    let config = client.get_config_json().await;

    Ok(Json(json!(cli_core::FederationConfigResponse {
        config: json!(config),
    })))
}

/// Get a federation's ecash balance.
async fn federation_balance(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationBalanceRequest>,
) -> Result<Json<Value>, GatewayError> {
    let client = state.select_client(req.federation_id).await?;

    let balance_msat = client.get_balance_for_btc().await?;

    Ok(Json(json!(cli_core::FederationBalanceResponse {
        balance_msat
    })))
}

// --- per-federation module commands ---

/// Count held ecash notes by denomination.
async fn mint_count(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationMintCountRequest>,
) -> Result<Json<Value>, GatewayError> {
    let client = state.select_client(req.federation_id).await?;

    let counts = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients")
        .get_count_by_denomination()
        .await;

    Ok(Json(json!(cli_core::FederationMintCountResponse {
        counts
    })))
}

/// Spend ecash from a federation.
async fn mint_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationMintSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    let client = state.select_client(req.federation_id).await?;

    let (_, ecash) = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients")
        .send(req.amount, serde_json::Value::Null, true)
        .await?;

    Ok(Json(json!(cli_core::FederationMintSendResponse {
        ecash: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
    })))
}

/// Receive ecash into the gateway. The ecash bundle itself carries the target
/// federation id prefix, so no federation argument is needed — but only a
/// federation whose client is already loaded can be targeted. Blocks until
/// issuance either completes or fails federation-side.
async fn mint_receive(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationMintReceiveRequest>,
) -> Result<Json<Value>, GatewayError> {
    let federation_id_prefix =
        base32::decode_prefixed::<fedimint_mintv2_client::ECash>(FEDIMINT_PREFIX, &req.ecash)
            .ok()
            .and_then(|ecash| ecash.mint())
            .map(|federation_id| federation_id.to_prefix())
            .ok_or_else(|| anyhow!("Invalid ecash format: could not parse as ECash"))?;

    let client = state
        .clients
        .read()
        .await
        .iter()
        .find_map(|(federation_id, client)| {
            (federation_id.to_prefix() == federation_id_prefix).then(|| client.clone())
        })
        .ok_or_else(|| anyhow!("No federation available for prefix {federation_id_prefix}"))?;

    let mint = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients");

    let ecash: fedimint_mintv2_client::ECash = base32::decode_prefixed(FEDIMINT_PREFIX, &req.ecash)
        .map_err(|e| anyhow!("Expected ECash for MintV2 federation: {e}"))?;
    let amount = ecash.amount();

    let operation_id = mint
        .receive(ecash, serde_json::Value::Null)
        .await
        .map_err(|e| anyhow!("{e}"))?;

    match mint
        .await_final_receive_operation_state(operation_id)
        .await
        .map_err(|e| anyhow!("{e}"))?
    {
        fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
        fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
            return Err(anyhow!("ECash receive was rejected").into());
        }
    }

    Ok(Json(json!(cli_core::FederationMintReceiveResponse {
        amount
    })))
}

/// Fetch the current onchain send-fee for a federation.
async fn wallet_send_fee(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationWalletSendFeeRequest>,
) -> Result<Json<Value>, GatewayError> {
    let client = state.select_client(req.federation_id).await?;

    let fee = client
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()?
        .send_fee()
        .await
        .map_err(|err| anyhow!("{err:?}"))?;

    Ok(Json(json!(cli_core::FederationWalletSendFeeResponse {
        fee
    })))
}

/// Withdraw onchain from a federation. Blocks until the send reaches a
/// terminal state. The cli-core `--fee` is not wired: walletv2 fetches the
/// current fee itself.
async fn wallet_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationWalletSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    let address_network = get_network_for_address(&req.address);
    let gateway_network = state.network;
    let Ok(address) = req.address.require_network(gateway_network) else {
        return Err(anyhow!(
            "Gateway is running on network {gateway_network}, but provided withdraw address is for network {address_network}"
        )
        .into());
    };

    let client = state.select_client(req.federation_id).await?;

    let wallet = client.get_first_module::<fedimint_walletv2_client::WalletClientModule>()?;

    let fee = wallet
        .send_fee()
        .await
        .map_err(|e| anyhow!("Error withdrawing funds onchain: {e}"))?;

    let operation_id = wallet
        .send(
            address.as_unchecked().clone(),
            req.amount,
            Some(fee),
            serde_json::Value::Null,
        )
        .await
        .map_err(|e| anyhow!("Error withdrawing funds onchain: {e}"))?;

    let result = wallet
        .await_final_send_operation_state(operation_id)
        .await
        .map_err(|e| anyhow!("Error withdrawing funds onchain: {e}"))?;

    match result {
        fedimint_walletv2_client::FinalSendOperationState::Success(txid) => {
            info!(target: LOG_GATEWAY, amount = %req.amount, address = %address, "Sent funds via walletv2");
            Ok(Json(json!(cli_core::FederationWalletSendResponse { txid })))
        }
        fedimint_walletv2_client::FinalSendOperationState::Aborted => {
            Err(anyhow!("Withdrawal transaction was aborted").into())
        }
        fedimint_walletv2_client::FinalSendOperationState::Failure => {
            Err(anyhow!("Withdrawal failed").into())
        }
    }
}

/// Generate a deposit address for a federation.
async fn wallet_receive(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationWalletReceiveRequest>,
) -> Result<Json<Value>, GatewayError> {
    let client = state.select_client(req.federation_id).await?;

    let address = client
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()?
        .receive()
        .await;

    Ok(Json(json!(cli_core::FederationWalletReceiveResponse {
        address: address.into_unchecked(),
    })))
}
