//! Admin server for `gatewaydv2`, served over a local Unix socket at
//! `{data_dir}/cli.sock` and driven by the `gatewaydv2-cli` binary.
//!
//! This is local-only (filesystem-permission gated), so unlike the HTTP API it
//! uses no bearer-token auth. Routes and payloads come from the
//! `fedimint-gatewayv2-cli-core` contract; picomint-style, each handler owns
//! its logic and operates on the LDK node / database / federation clients
//! directly, returning the matching cli-core response, which the CLI
//! pretty-prints. A handler refuses a request with [`CliError::rejected`]
//! and one of cli-core's error enums, whose variant is the code the CLI
//! prints; an anyhow error reaching a handler is a bug and surfaces as
//! `internal`.

use std::collections::HashMap;
use std::fs::{Permissions, remove_file, set_permissions};
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener as StdUnixListener;
use std::time::Duration;

use anyhow::Context as _;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use bitcoin::FeeRate;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::db::IDatabaseTransactionOpsCoreTyped as _;
use fedimint_core::error::ErrorCode;
use fedimint_gatewayv2_cli_core as cli_core;
use fedimint_gatewayv2_cli_core::{
    LdkError, LdkReceiveError, LdkSendError, MintReceiveError, MintSendError, NotJoinedError,
    WalletSendError, WalletSendFeeError,
};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule as MintV2ClientModule;
use fedimint_walletv2_client::WalletClientModule;
use hex::ToHex as _;
use ldk_node::UserChannelId;
use ldk_node::lightning::routing::gossip::NodeId;
use ldk_node::payment::{PaymentKind, PaymentStatus};
use lightning_invoice::{Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description};
use serde::Serialize;
use tokio::net::UnixListener;
use tracing::{info, instrument};

use crate::db::{ClientConfigKey, DisabledFederationKey};
use crate::{AppState, analytics, client};

/// What an admin handler fails with, and what the CLI prints: a status, a
/// stable `code` the caller branches on and the `error` message it shows
/// the operator. The body is `{"code": ..., "error": ...}`.
#[derive(Debug, Serialize)]
pub struct CliError {
    #[serde(skip)]
    pub status: StatusCode,
    pub code: &'static str,
    pub error: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.error)
    }
}

impl CliError {
    /// A request the daemon refuses for a reason the caller can act on: the
    /// enum's variant is the code, its message the error. Every error a
    /// handler returns on purpose goes through here, so the CLI's `--help`
    /// can list the codes from the same enum. Takes the error by value so a
    /// handler can pass it straight through `map_err`.
    #[allow(clippy::needless_pass_by_value)]
    pub fn rejected(error: impl ErrorCode + std::fmt::Display) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            code: error.code(),
            error: error.to_string(),
        }
    }

    /// A failure nothing typed: an anyhow error reaching a handler, which is
    /// a bug in the daemon rather than a rejection of the request.
    pub fn internal(error: impl std::fmt::Display) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            code: "internal",
            error: error.to_string(),
        }
    }
}

impl IntoResponse for CliError {
    fn into_response(self) -> axum::response::Response {
        (self.status, Json(&self)).into_response()
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(e)
    }
}

/// Bind the admin socket at `{data_dir}/cli.sock` and return the future that
/// serves the admin router on it until the process exits. A stale socket from
/// a previous (crashed) run is unlinked before binding.
///
/// The bind happens here, synchronously, rather than inside the returned
/// future: `main` spawns the future, so a failure inside it would leave the
/// daemon running with no admin surface, whereas a failure on the daemon's
/// main path ends the process.
///
/// Every route on the socket is full custody and nothing on it authenticates
/// the peer, so the file modes are the only gate. They are set explicitly
/// rather than inherited from the umask: the data dir goes owner-only first,
/// which also covers the databases beside the socket and closes the window
/// between bind and chmod.
pub fn run(state: AppState) -> anyhow::Result<impl Future<Output = ()> + use<>> {
    let data_dir = state.data_dir.clone();

    set_permissions(&data_dir, Permissions::from_mode(0o700))
        .with_context(|| format!("Failed to make {} owner-only", data_dir.display()))?;

    let socket_path = data_dir.join(cli_core::CLI_SOCKET_FILENAME);

    remove_file(&socket_path).ok();

    let listener = StdUnixListener::bind(&socket_path).with_context(|| {
        format!(
            "Failed to bind the admin socket at {}",
            socket_path.display()
        )
    })?;

    set_permissions(&socket_path, Permissions::from_mode(0o600))?;

    listener.set_nonblocking(true)?;

    info!(target: LOG_GATEWAY, socket = %socket_path.display(), "Bound the gatewaydv2 admin socket");

    let router = router().with_state(state);

    Ok(async move {
        let listener = UnixListener::from_std(listener).expect("called within a tokio runtime");

        axum::serve(listener, router.into_make_service())
            .await
            .expect("Admin socket server failed");
    })
}

fn router() -> Router<AppState> {
    Router::new()
        .route(cli_core::ROUTE_INFO, post(info))
        .route(cli_core::ROUTE_MNEMONIC, post(mnemonic))
        .route(cli_core::ROUTE_QUERY, post(query))
        .route(cli_core::ROUTE_LDK_BALANCES, post(ldk_balances))
        .route(
            cli_core::ROUTE_LDK_ONCHAIN_RECEIVE,
            post(ldk_onchain_receive),
        )
        .route(cli_core::ROUTE_LDK_ONCHAIN_SEND, post(ldk_onchain_send))
        .route(cli_core::ROUTE_LDK_CHANNEL_OPEN, post(ldk_channel_open))
        .route(cli_core::ROUTE_LDK_CHANNEL_CLOSE, post(ldk_channel_close))
        .route(cli_core::ROUTE_LDK_CHANNEL_LIST, post(ldk_channel_list))
        .route(
            cli_core::ROUTE_LDK_CHANNEL_SPLICE_IN,
            post(ldk_channel_splice_in),
        )
        .route(
            cli_core::ROUTE_LDK_CHANNEL_SPLICE_OUT,
            post(ldk_channel_splice_out),
        )
        .route(cli_core::ROUTE_LDK_LN_RECEIVE, post(ldk_ln_receive))
        .route(cli_core::ROUTE_LDK_LN_SEND, post(ldk_ln_send))
        .route(cli_core::ROUTE_LDK_LN_PROBE, post(ldk_ln_probe))
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

/// The federation's client, or the one reason there is none: the
/// federation is not joined, or its client failed to load, which
/// [`AppState::select_client`] logs.
async fn select_client(
    state: &AppState,
    federation_id: fedimint_core::config::FederationId,
) -> Result<fedimint_client::ClientHandleArc, NotJoinedError> {
    state
        .select_client(federation_id)
        .await
        .map_err(|_| NotJoinedError::NotJoined)
}

// --- top-level ---

/// Display high-level information about the gateway.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn info(State(state): State<AppState>) -> Result<Json<cli_core::InfoResponse>, CliError> {
    let node_status = state.node.status();

    Ok(Json(cli_core::InfoResponse {
        lightning_pk: state.node.node_id(),
        network: state.network.to_string(),
        block_height: u64::from(node_status.current_best_block.height),
        synced_to_chain: node_status.latest_lightning_wallet_sync_timestamp.is_some(),
    }))
}

/// Returns the gateway's mnemonic words.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mnemonic(
    State(state): State<AppState>,
) -> Result<Json<cli_core::MnemonicResponse>, CliError> {
    let mnemonic = client::load_mnemonic(&state.gateway_db)
        .await
        .expect("mnemonic should be set");

    let words = mnemonic
        .words()
        .map(std::string::ToString::to_string)
        .collect::<Vec<_>>();

    Ok(Json(cli_core::MnemonicResponse { mnemonic: words }))
}

/// Runs read-only SQL against the analytics db.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn query(
    State(state): State<AppState>,
    Json(req): Json<cli_core::QueryRequest>,
) -> Result<Json<cli_core::QueryResponse>, CliError> {
    let rows = tokio::task::spawn_blocking(move || analytics::query(&state.data_dir, &req.query))
        .await
        .expect("the query task is not cancelled")
        .map_err(CliError::rejected)?;

    Ok(Json(cli_core::QueryResponse(rows)))
}

// --- ldk node management ---

/// Returns the onchain and lightning channel capacity balances.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_balances(
    State(state): State<AppState>,
) -> Result<Json<cli_core::LdkBalancesResponse>, CliError> {
    let balances = state.node.list_balances();

    // A channel that is not usable — still awaiting its funding confirmation,
    // or its peer disconnected — carries no payment in either direction, so it
    // contributes to none of the three capacities below.
    let usable_channels = state
        .node
        .list_channels()
        .into_iter()
        .filter(|channel| channel.is_usable)
        .collect::<Vec<_>>();

    let total_inbound_capacity_sat: u64 = usable_channels
        .iter()
        .map(|channel| channel.inbound_capacity_msat / 1000)
        .sum();

    let total_outbound_capacity_sat: u64 = usable_channels
        .iter()
        .map(|channel| channel.outbound_capacity_msat / 1000)
        .sum();

    let total_next_outbound_htlc_limit_sat: u64 = usable_channels
        .iter()
        .map(|channel| channel.next_outbound_htlc_limit_msat / 1000)
        .sum();

    Ok(Json(cli_core::LdkBalancesResponse {
        total_onchain_balance_sat: balances.total_onchain_balance_sats,
        spendable_onchain_balance_sat: balances.spendable_onchain_balance_sats,
        total_anchor_channels_reserve_sat: balances.total_anchor_channels_reserve_sats,
        total_lightning_balance_sat: balances.total_lightning_balance_sats,
        total_inbound_capacity_sat,
        total_outbound_capacity_sat,
        total_next_outbound_htlc_limit_sat,
    }))
}

/// Generates an onchain address to fund the gateway's lightning node.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_receive(
    State(state): State<AppState>,
) -> Result<Json<cli_core::LdkOnchainReceiveResponse>, CliError> {
    let address = state
        .node
        .onchain_payment()
        .new_address()
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    Ok(Json(cli_core::LdkOnchainReceiveResponse {
        address: address.as_unchecked().clone(),
    }))
}

/// Send funds from the gateway's lightning node on-chain wallet.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_onchain_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkOnchainSendRequest>,
) -> Result<Json<cli_core::LdkOnchainSendResponse>, CliError> {
    let txid = state
        .node
        .onchain_payment()
        .send_to_address(
            &req.address.assume_checked(),
            req.amount.to_sat(),
            FeeRate::from_sat_per_vb(req.sat_per_vbyte),
        )
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    info!(target: LOG_GATEWAY, txid = %txid, "Sent onchain transaction");

    Ok(Json(cli_core::LdkOnchainSendResponse { txid }))
}

/// Opens a Lightning channel to a peer. Fire-and-forget, picomint-style: the
/// funding transaction is negotiated and broadcast asynchronously; callers
/// observe it via the channel list.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_open(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkChannelOpenRequest>,
) -> Result<Json<()>, CliError> {
    let push_amount_msat = if req.push_amount_sat == 0 {
        None
    } else {
        Some(req.push_amount_sat * 1000)
    };

    // Unannounced by default, matching LDK; a gateway only needs its peers to
    // route to it, not the wider network.
    let open_channel = if req.announce {
        ldk_node::Node::open_announced_channel
    } else {
        ldk_node::Node::open_channel
    };

    open_channel(
        &state.node,
        req.pubkey,
        req.host,
        req.channel_size_sat,
        push_amount_msat,
        None,
    )
    .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    info!(
        target: LOG_GATEWAY,
        pubkey = %req.pubkey,
        announce = req.announce,
        "Initiated channel open"
    );

    Ok(Json(()))
}

/// Closes a channel.
///
/// The channel is named by its `user_channel_id` rather than by peer, since a
/// peer may hold several; `channel list` reports both fields.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_close(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkChannelCloseRequest>,
) -> Result<Json<()>, CliError> {
    let user_channel_id = UserChannelId(req.user_channel_id);

    if req.force {
        state
            .node
            .force_close_channel(
                &user_channel_id,
                req.pubkey,
                Some("User initiated force close".to_string()),
            )
            .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;
    } else {
        state
            .node
            .close_channel(&user_channel_id, req.pubkey)
            .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;
    }

    info!(
        target: LOG_GATEWAY,
        user_channel_id = req.user_channel_id,
        pubkey = %req.pubkey,
        force = req.force,
        "Initiated channel closure"
    );

    Ok(Json(()))
}

/// Splices on-chain funds into the channel with a peer, growing its capacity
/// without closing it. Experimental; the counterparty must support splicing.
///
/// The channel is named by its `user_channel_id` rather than by peer, since a
/// peer may hold several; `channel list` reports both fields.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_splice_in(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkChannelSpliceInRequest>,
) -> Result<Json<()>, CliError> {
    state
        .node
        .splice_in(
            &UserChannelId(req.user_channel_id),
            req.pubkey,
            req.amount_sat,
        )
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    info!(
        target: LOG_GATEWAY,
        user_channel_id = req.user_channel_id,
        pubkey = %req.pubkey,
        amount_sat = req.amount_sat,
        "Initiated splice-in"
    );

    Ok(Json(()))
}

/// Splices funds out of a channel to an on-chain address without closing it.
/// Experimental; the amount must not exceed the channel's outbound capacity
/// and the counterparty must support splicing.
///
/// The channel is named by its `user_channel_id` rather than by peer, since a
/// peer may hold several; `channel list` reports both fields.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_splice_out(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkChannelSpliceOutRequest>,
) -> Result<Json<()>, CliError> {
    state
        .node
        .splice_out(
            &UserChannelId(req.user_channel_id),
            req.pubkey,
            &req.address.assume_checked(),
            req.amount_sat,
        )
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    info!(
        target: LOG_GATEWAY,
        user_channel_id = req.user_channel_id,
        pubkey = %req.pubkey,
        amount_sat = req.amount_sat,
        "Initiated splice-out"
    );

    Ok(Json(()))
}

/// Lists all Lightning channels.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_channel_list(
    State(state): State<AppState>,
) -> Result<Json<cli_core::LdkChannelListResponse>, CliError> {
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
            user_channel_id: channel_details.user_channel_id.0,
            remote_pubkey: channel_details.counterparty_node_id,
            remote_alias,
            remote_address,
            channel_size_sat: channel_details.channel_value_sats,
            outbound_liquidity_sat: channel_details.outbound_capacity_msat / 1000,
            next_outbound_htlc_limit_sat: channel_details.next_outbound_htlc_limit_msat / 1000,
            inbound_liquidity_sat: channel_details.inbound_capacity_msat / 1000,
            is_usable: channel_details.is_usable,
            is_outbound: channel_details.is_outbound,
            is_announced: channel_details.is_announced,
            funding_txid: channel_details.funding_txo.map(|txo| txo.txid),
        });
    }

    Ok(Json(cli_core::LdkChannelListResponse { channels }))
}

/// Creates an invoice directly payable to the gateway's lightning node.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_ln_receive(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkLnReceiveRequest>,
) -> Result<Json<cli_core::LdkLnReceiveResponse>, CliError> {
    let expiry_secs = req.expiry_secs.unwrap_or(3600);

    let description = match req.description {
        Some(description) => LdkBolt11InvoiceDescription::Direct(
            Description::new(description)
                .map_err(|_| CliError::rejected(LdkReceiveError::InvalidDescription))?,
        ),
        None => LdkBolt11InvoiceDescription::Direct(Description::empty()),
    };

    let invoice = state
        .node
        .bolt11_payment()
        .receive(req.amount_msat, &description, expiry_secs)
        .map_err(|e| CliError::rejected(LdkReceiveError::Ldk(e.to_string())))?;

    Ok(Json(cli_core::LdkLnReceiveResponse {
        invoice: invoice.to_string(),
    }))
}

/// Pays an outgoing LN invoice using the gateway's own funds.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_ln_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkLnSendRequest>,
) -> Result<Json<cli_core::LdkLnSendResponse>, CliError> {
    let payment_id = state
        .node
        .bolt11_payment()
        .send(&req.invoice, None)
        .map_err(|e| CliError::rejected(LdkSendError::Ldk(e.to_string())))?;

    let preimage: [u8; 32] = loop {
        if let Some(payment_details) = state.node.payment(&payment_id) {
            match payment_details.status {
                PaymentStatus::Pending => {}
                PaymentStatus::Succeeded => {
                    if let PaymentKind::Bolt11 {
                        preimage: Some(preimage),
                        ..
                    } = payment_details.kind
                    {
                        break preimage.0;
                    }
                }
                PaymentStatus::Failed => {
                    return Err(CliError::rejected(LdkSendError::PaymentFailed));
                }
            }
        }
        fedimint_core::runtime::sleep(Duration::from_millis(100)).await;
    };

    Ok(Json(cli_core::LdkLnSendResponse {
        preimage: preimage.encode_hex::<String>(),
    }))
}

/// Sends payment probes over all routes towards a node for the given amount,
/// to exercise pathfinding and warm the scorer without moving funds. Probe
/// outcomes surface only in the daemon's LDK logs (the `Got route` and
/// `Onion Error` lines), so nothing meaningful is returned here.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_ln_probe(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkLnProbeRequest>,
) -> Result<Json<()>, CliError> {
    state
        .node
        .spontaneous_payment()
        .send_probes(req.amount_msat, req.node_id)
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    Ok(Json(()))
}

/// Connects to a Lightning peer, persisting the connection so the node
/// reconnects on restart.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_connect(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkPeerConnectRequest>,
) -> Result<Json<()>, CliError> {
    state
        .node
        .connect(req.pubkey, req.host, true)
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    info!(target: LOG_GATEWAY, pubkey = %req.pubkey, "Connected to peer");

    Ok(Json(()))
}

/// Disconnects from a Lightning peer.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_disconnect(
    State(state): State<AppState>,
    Json(req): Json<cli_core::LdkPeerDisconnectRequest>,
) -> Result<Json<()>, CliError> {
    state
        .node
        .disconnect(req.pubkey)
        .map_err(|e| CliError::rejected(LdkError::Ldk(e.to_string())))?;

    info!(target: LOG_GATEWAY, pubkey = %req.pubkey, "Disconnected from peer");

    Ok(Json(()))
}

/// Lists all Lightning peers.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn ldk_peer_list(
    State(state): State<AppState>,
) -> Result<Json<cli_core::LdkPeerListResponse>, CliError> {
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

    Ok(Json(cli_core::LdkPeerListResponse { peers }))
}

// --- federation management ---

/// Join a new federation: download and persist its config; the client itself
/// is built lazily on first use.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_join(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationJoinRequest>,
) -> Result<Json<()>, CliError> {
    state
        .connect_federation(req.invite)
        .await
        .map_err(CliError::rejected)?;

    Ok(Json(()))
}

/// Disable a federation's public client API. Its config and client state are
/// retained so in-flight payments settle and it can be re-enabled.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_disable(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationDisableRequest>,
) -> Result<Json<()>, CliError> {
    let mut dbtx = state.gateway_db.begin_transaction().await;

    dbtx.get_value(&ClientConfigKey(req.federation_id))
        .await
        .ok_or_else(|| CliError::rejected(NotJoinedError::NotJoined))?;

    dbtx.insert_entry(&DisabledFederationKey(req.federation_id), &())
        .await;

    dbtx.commit_tx().await;

    Ok(Json(()))
}

/// Re-enable a previously disabled federation. Blind remove — no-op if the
/// row isn't there.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_enable(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationEnableRequest>,
) -> Result<Json<()>, CliError> {
    let mut dbtx = state.gateway_db.begin_transaction().await;
    dbtx.remove_entry(&DisabledFederationKey(req.federation_id))
        .await;
    dbtx.commit_tx().await;

    Ok(Json(()))
}

/// List joined federations.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_list(
    State(state): State<AppState>,
) -> Result<Json<cli_core::FederationListResponse>, CliError> {
    let federations = state.federation_list().await;

    Ok(Json(cli_core::FederationListResponse { federations }))
}

/// Display federation config.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_config(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationConfigRequest>,
) -> Result<Json<cli_core::FederationConfigResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(CliError::rejected)?;

    let config = client.get_config_json().await;

    Ok(Json(cli_core::FederationConfigResponse {
        config: serde_json::to_value(config).expect("a client config serializes"),
    }))
}

/// Get the gateway's ecash balance in a federation.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn federation_balance(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationBalanceRequest>,
) -> Result<Json<cli_core::FederationBalanceResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(CliError::rejected)?;

    let balance_msat = client.get_balance_for_btc().await?;

    Ok(Json(cli_core::FederationBalanceResponse { balance_msat }))
}

// --- per-federation module commands ---

/// Count held ecash notes by denomination.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mint_count(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationMintCountRequest>,
) -> Result<Json<cli_core::FederationMintCountResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(CliError::rejected)?;

    let counts = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients")
        .get_count_by_denomination()
        .await;

    Ok(Json(cli_core::FederationMintCountResponse { counts }))
}

/// Spend ecash from a federation.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mint_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationMintSendRequest>,
) -> Result<Json<cli_core::FederationMintSendResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(|e| CliError::rejected(MintSendError::from(e)))?;

    let (_, ecash) = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients")
        .send(req.amount, serde_json::Value::Null, true)
        .await
        .map_err(|e| CliError::rejected(MintSendError::from(e)))?;

    Ok(Json(cli_core::FederationMintSendResponse {
        ecash: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
    }))
}

/// Receive ecash into the gateway. Blocks until issuance either completes or
/// fails federation-side.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn mint_receive(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationMintReceiveRequest>,
) -> Result<Json<cli_core::FederationMintReceiveResponse>, CliError> {
    let ecash: fedimint_mintv2_client::ECash = base32::decode_prefixed(FEDIMINT_PREFIX, &req.ecash)
        .map_err(|e| CliError::rejected(MintReceiveError::InvalidEcash(e.to_string())))?;

    let client = select_client(&state, req.federation_id)
        .await
        .map_err(|e| CliError::rejected(MintReceiveError::from(e)))?;

    let mint = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients");

    let amount = ecash.amount();

    let operation_id = mint
        .receive(ecash, serde_json::Value::Null)
        .await
        .map_err(|e| CliError::rejected(MintReceiveError::from(e)))?;

    match mint
        .await_final_receive_operation_state(operation_id)
        .await?
    {
        fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
        fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
            return Err(CliError::rejected(MintReceiveError::Rejected));
        }
    }

    Ok(Json(cli_core::FederationMintReceiveResponse { amount }))
}

/// Fetch the current onchain send-fee for a federation.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn wallet_send_fee(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationWalletSendFeeRequest>,
) -> Result<Json<cli_core::FederationWalletSendFeeResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(|e| CliError::rejected(WalletSendFeeError::from(e)))?;

    let fee = client
        .get_first_module::<WalletClientModule>()
        .expect("WalletV2 module is always attached to gateway clients")
        .send_fee()
        .await
        .map_err(|e| CliError::rejected(WalletSendFeeError::from(e)))?;

    Ok(Json(cli_core::FederationWalletSendFeeResponse { fee }))
}

/// Withdraw onchain from a federation. Blocks until the send reaches a
/// terminal state. `--fee` overrides the federation's fee quote; without it
/// walletv2 fetches the current one itself.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn wallet_send(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationWalletSendRequest>,
) -> Result<Json<cli_core::FederationWalletSendResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(|e| CliError::rejected(WalletSendError::from(e)))?;

    let wallet = client
        .get_first_module::<WalletClientModule>()
        .expect("WalletV2 module is always attached to gateway clients");

    let operation_id = wallet
        .send(
            req.address.clone(),
            req.amount,
            req.fee,
            serde_json::Value::Null,
        )
        .await
        .map_err(|e| CliError::rejected(WalletSendError::from(e)))?;

    match wallet
        .await_final_send_operation_state(operation_id)
        .await?
    {
        fedimint_walletv2_client::FinalSendOperationState::Success(txid) => {
            info!(
                target: LOG_GATEWAY,
                amount = %req.amount,
                address = %req.address.assume_checked_ref(),
                "Sent funds via walletv2"
            );

            Ok(Json(cli_core::FederationWalletSendResponse { txid }))
        }
        fedimint_walletv2_client::FinalSendOperationState::Aborted => {
            Err(CliError::rejected(WalletSendError::Aborted))
        }
        fedimint_walletv2_client::FinalSendOperationState::Failure => {
            Err(CliError::rejected(WalletSendError::Failure))
        }
    }
}

/// Generate a deposit address for a federation.
#[instrument(target = LOG_GATEWAY, skip_all, err)]
async fn wallet_receive(
    State(state): State<AppState>,
    Json(req): Json<cli_core::FederationWalletReceiveRequest>,
) -> Result<Json<cli_core::FederationWalletReceiveResponse>, CliError> {
    let client = select_client(&state, req.federation_id)
        .await
        .map_err(CliError::rejected)?;

    let address = client
        .get_first_module::<WalletClientModule>()
        .expect("WalletV2 module is always attached to gateway clients")
        .receive()
        .await;

    Ok(Json(cli_core::FederationWalletReceiveResponse {
        address: address.into_unchecked(),
    }))
}
