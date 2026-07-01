//! Admin server for `gatewaydv2`, served over a local Unix socket at
//! `{data_dir}/cli.sock` and driven by the `gatewaydv2-cli` binary.
//!
//! This is local-only (filesystem-permission gated), so unlike the HTTP API it
//! uses no bearer-token auth. Routes and payloads come from the
//! `fedimint-gatewayv2-cli-core` contract; each handler maps onto an existing
//! gateway method (or a small helper here) and returns the matching cli-core
//! response, which the CLI pretty-prints.

use std::collections::BTreeMap;
use std::sync::Arc;

use axum::routing::post;
use axum::{Extension, Json, Router};
use fedimint_core::config::FederationId;
use fedimint_core::{Amount, BitcoinAmountOrAll};
use fedimint_gateway_common::{
    CloseChannelsWithPeerRequest, CreateInvoiceForOperatorPayload, DepositAddressPayload,
    LeaveFedPayload, OpenChannelRequest, PayInvoiceForOperatorPayload, ReceiveEcashPayload,
    SendOnchainRequest, SpendEcashPayload,
};
use fedimint_gatewayv2_cli_core as cli;
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule as MintV2ClientModule;
use fedimint_mintv2_common::Denomination;
use hex::ToHex;
use serde_json::{Value, json};
use tokio::net::UnixListener;
use tracing::info;

use crate::db::GatewayDbtxNcExt as _;
use crate::error::GatewayError;
use crate::{AdminResult, Gateway};

/// Runs the admin server over a local Unix socket until the task is aborted (on
/// process shutdown). Mirrors [`crate::rpc_server::run_public`] but over a
/// `UnixListener`. Spawned as a fire-and-forget task from `main`.
pub async fn run_cli(gateway: Gateway) -> anyhow::Result<()> {
    let socket_path = gateway
        .client_builder
        .data_dir()
        .join(cli::CLI_SOCKET_FILENAME);
    // Remove any stale socket left by a previous run before binding.
    let _ = std::fs::remove_file(&socket_path);

    let router = router(Arc::new(gateway));

    let listener = UnixListener::bind(&socket_path)?;
    info!(target: LOG_GATEWAY, socket = %socket_path.display(), "Started gatewaydv2 cli server");
    axum::serve(listener, router.into_make_service()).await?;

    Ok(())
}

fn router(gateway: Arc<Gateway>) -> Router {
    Router::new()
        .route(cli::ROUTE_INFO, post(info))
        .route(cli::ROUTE_MNEMONIC, post(mnemonic))
        .route(cli::ROUTE_LDK_BALANCES, post(ldk_balances))
        .route(cli::ROUTE_LDK_ONCHAIN_RECEIVE, post(ldk_onchain_receive))
        .route(cli::ROUTE_LDK_ONCHAIN_SEND, post(ldk_onchain_send))
        .route(cli::ROUTE_LDK_CHANNEL_OPEN, post(ldk_channel_open))
        .route(cli::ROUTE_LDK_CHANNEL_CLOSE, post(ldk_channel_close))
        .route(cli::ROUTE_LDK_CHANNEL_LIST, post(ldk_channel_list))
        .route(cli::ROUTE_LDK_LN_RECEIVE, post(ldk_ln_receive))
        .route(cli::ROUTE_LDK_LN_SEND, post(ldk_ln_send))
        .route(cli::ROUTE_LDK_PEER_CONNECT, post(ldk_peer_connect))
        .route(cli::ROUTE_LDK_PEER_DISCONNECT, post(ldk_peer_disconnect))
        .route(cli::ROUTE_LDK_PEER_LIST, post(ldk_peer_list))
        .route(cli::ROUTE_FEDERATION_JOIN, post(federation_join))
        .route(cli::ROUTE_FEDERATION_DISABLE, post(federation_disable))
        .route(cli::ROUTE_FEDERATION_ENABLE, post(federation_enable))
        .route(cli::ROUTE_FEDERATION_LIST, post(federation_list))
        .route(cli::ROUTE_FEDERATION_CONFIG, post(federation_config))
        .route(cli::ROUTE_FEDERATION_BALANCE, post(federation_balance))
        .route(cli::ROUTE_FEDERATION_MODULE_MINT_COUNT, post(mint_count))
        .route(cli::ROUTE_FEDERATION_MODULE_MINT_SEND, post(mint_send))
        .route(
            cli::ROUTE_FEDERATION_MODULE_MINT_RECEIVE,
            post(mint_receive),
        )
        .route(
            cli::ROUTE_FEDERATION_MODULE_WALLET_SEND_FEE,
            post(wallet_send_fee),
        )
        .route(cli::ROUTE_FEDERATION_MODULE_WALLET_SEND, post(wallet_send))
        .route(
            cli::ROUTE_FEDERATION_MODULE_WALLET_RECEIVE,
            post(wallet_receive),
        )
        .layer(Extension(gateway))
}

// --- top-level ---

async fn info(Extension(gateway): Extension<Arc<Gateway>>) -> Result<Json<Value>, GatewayError> {
    let node = gateway.info();
    Ok(Json(json!(cli::InfoResponse {
        lightning_pk: node.pub_key,
        network: gateway.network.to_string(),
        block_height: u64::from(node.block_height),
        synced_to_chain: node.synced_to_chain,
    })))
}

async fn mnemonic(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<Value>, GatewayError> {
    let response = gateway.handle_mnemonic_msg().await?;
    Ok(Json(json!(response)))
}

// --- ldk ---

async fn ldk_balances(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<Value>, GatewayError> {
    let balances = gateway.get_balances();
    let channels = gateway.list_channels();
    let total_outbound_capacity_msat = channels
        .iter()
        .map(|channel| channel.outbound_liquidity_sats * 1000)
        .sum();
    Ok(Json(json!(cli::LdkBalancesResponse {
        total_onchain_balance_sat: balances.onchain_balance_sats,
        total_inbound_capacity_msat: balances.inbound_lightning_liquidity_msats,
        total_outbound_capacity_msat,
    })))
}

async fn ldk_onchain_receive(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<Value>, GatewayError> {
    let address = gateway.handle_get_ln_onchain_address_msg()?;
    Ok(Json(json!(cli::LdkOnchainReceiveResponse {
        address: address.into_unchecked(),
    })))
}

async fn ldk_onchain_send(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkOnchainSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    let txid = gateway.handle_send_onchain_msg(&SendOnchainRequest {
        address: req.address,
        amount: BitcoinAmountOrAll::Amount(req.amount),
        fee_rate_sats_per_vbyte: req.sat_per_vbyte,
    })?;
    Ok(Json(json!(cli::LdkOnchainSendResponse { txid })))
}

async fn ldk_channel_open(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkChannelOpenRequest>,
) -> Result<Json<Value>, GatewayError> {
    let funding_txid = gateway
        .handle_open_channel_msg(OpenChannelRequest {
            pubkey: req.pubkey,
            host: req.host,
            channel_size_sats: req.channel_size_sat,
            push_amount_sats: req.push_amount_sat,
            fee_rate_sats_per_vbyte: None,
            base_fee_msat: None,
            parts_per_million: None,
        })
        .await?;
    // The cli-core contract has no open-channel response; return the funding
    // txid, which is more useful than an empty body.
    Ok(Json(json!({ "funding_txid": funding_txid })))
}

async fn ldk_channel_close(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkChannelCloseRequest>,
) -> Result<Json<Value>, GatewayError> {
    let response = gateway.handle_close_channels_with_peer_msg(&CloseChannelsWithPeerRequest {
        pubkey: req.pubkey,
        force: req.force,
        sats_per_vbyte: req.sat_per_vbyte,
    });
    Ok(Json(json!(cli::LdkChannelCloseResponse {
        num_channels_closed: response.num_channels_closed,
    })))
}

async fn ldk_channel_list(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<Value>, GatewayError> {
    let channels = gateway
        .handle_list_channels_msg()
        .into_iter()
        .map(|channel| cli::ChannelInfo {
            remote_pubkey: channel.remote_pubkey,
            remote_alias: channel.remote_node_alias,
            remote_address: channel.remote_address,
            channel_size_sat: channel.channel_size_sats,
            outbound_liquidity_sat: channel.outbound_liquidity_sats,
            inbound_liquidity_sat: channel.inbound_liquidity_sats,
            is_usable: channel.is_active,
            // Not available from the shared ChannelInfo; defaulted.
            is_outbound: false,
            funding_txid: channel.funding_outpoint.map(|outpoint| outpoint.txid),
        })
        .collect();
    Ok(Json(json!(cli::LdkChannelListResponse { channels })))
}

async fn ldk_ln_receive(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkLnReceiveRequest>,
) -> Result<Json<Value>, GatewayError> {
    let invoice =
        gateway.handle_create_invoice_for_operator_msg(CreateInvoiceForOperatorPayload {
            amount_msats: req.amount_msat,
            expiry_secs: req.expiry_secs,
            description: req.description,
        })?;
    Ok(Json(json!(cli::LdkLnReceiveResponse {
        invoice: invoice.to_string(),
    })))
}

async fn ldk_ln_send(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkLnSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    let preimage = gateway
        .handle_pay_invoice_for_operator_msg(PayInvoiceForOperatorPayload {
            invoice: req.invoice,
        })
        .await?;
    Ok(Json(json!(cli::LdkLnSendResponse {
        preimage: preimage.0.encode_hex::<String>(),
    })))
}

async fn ldk_peer_connect(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkPeerConnectRequest>,
) -> Result<Json<Value>, GatewayError> {
    gateway.connect_peer(req.pubkey, &req.host)?;
    Ok(Json(json!(())))
}

async fn ldk_peer_disconnect(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::LdkPeerDisconnectRequest>,
) -> Result<Json<Value>, GatewayError> {
    gateway.disconnect_peer(req.pubkey)?;
    Ok(Json(json!(())))
}

async fn ldk_peer_list(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<Value>, GatewayError> {
    let peers = gateway
        .list_peers()
        .into_iter()
        .map(|(node_id, address, is_connected)| cli::PeerInfo {
            node_id,
            address,
            is_connected,
        })
        .collect();
    Ok(Json(json!(cli::LdkPeerListResponse { peers })))
}

// --- federation ---

async fn federation_join(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationJoinRequest>,
) -> Result<Json<Value>, GatewayError> {
    gateway.handle_connect_federation(req.invite).await?;
    Ok(Json(json!(())))
}

async fn federation_disable(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationDisableRequest>,
) -> Result<Json<Value>, GatewayError> {
    gateway
        .handle_leave_federation(LeaveFedPayload {
            federation_id: req.federation_id,
        })
        .await?;
    Ok(Json(json!(())))
}

async fn federation_enable(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationEnableRequest>,
) -> Result<Json<Value>, GatewayError> {
    let mut dbtx = gateway.gateway_db.begin_transaction().await;
    dbtx.remove_disabled_federation(req.federation_id).await;
    dbtx.commit_tx().await;
    Ok(Json(json!(())))
}

async fn federation_list(
    Extension(gateway): Extension<Arc<Gateway>>,
) -> Result<Json<Value>, GatewayError> {
    let federations = gateway.list_federation_infos().await;
    Ok(Json(json!(cli::FederationListResponse { federations })))
}

async fn federation_config(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationConfigRequest>,
) -> Result<Json<Value>, GatewayError> {
    let response = gateway
        .handle_get_federation_config(Some(req.federation_id))
        .await?;
    let config = response
        .federations
        .get(&req.federation_id)
        .map_or(Value::Null, |config| json!(config));
    Ok(Json(json!(cli::FederationConfigResponse { config })))
}

async fn federation_balance(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationBalanceRequest>,
) -> Result<Json<Value>, GatewayError> {
    let balance_msat = balance_for(&gateway, req.federation_id).await?;
    Ok(Json(json!(cli::FederationBalanceResponse { balance_msat })))
}

async fn mint_count(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationMintCountRequest>,
) -> Result<Json<Value>, GatewayError> {
    let counts = mint_counts(&gateway, req.federation_id).await?;
    Ok(Json(json!(cli::FederationMintCountResponse { counts })))
}

async fn mint_send(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationMintSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    let response = gateway
        .handle_spend_ecash_msg(SpendEcashPayload {
            federation_id: req.federation_id,
            amount: req.amount,
        })
        .await?;
    Ok(Json(json!(cli::FederationMintSendResponse {
        ecash: response.notes,
    })))
}

async fn mint_receive(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationMintReceiveRequest>,
) -> Result<Json<Value>, GatewayError> {
    let response = gateway
        .handle_receive_ecash_msg(ReceiveEcashPayload {
            notes: req.ecash,
            wait: true,
        })
        .await?;
    Ok(Json(json!(cli::FederationMintReceiveResponse {
        amount: response.amount,
    })))
}

async fn wallet_send_fee(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationWalletSendFeeRequest>,
) -> Result<Json<Value>, GatewayError> {
    let fee = wallet_send_fee_for(&gateway, req.federation_id).await?;
    Ok(Json(json!(cli::FederationWalletSendFeeResponse { fee })))
}

async fn wallet_send(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationWalletSendRequest>,
) -> Result<Json<Value>, GatewayError> {
    // The cli-core `--fee` is not wired: walletv2 fetches current fees itself.
    let response = gateway
        .handle_withdraw_msg(req.federation_id, req.address, req.amount)
        .await?;
    Ok(Json(json!(cli::FederationWalletSendResponse {
        txid: response.txid,
    })))
}

async fn wallet_receive(
    Extension(gateway): Extension<Arc<Gateway>>,
    Json(req): Json<cli::FederationWalletReceiveRequest>,
) -> Result<Json<Value>, GatewayError> {
    let address = gateway
        .handle_address_msg(DepositAddressPayload {
            federation_id: req.federation_id,
        })
        .await?;
    Ok(Json(json!(cli::FederationWalletReceiveResponse {
        address: address.into_unchecked(),
    })))
}

// --- helpers for operations without an existing gateway method ---

async fn balance_for(gateway: &Gateway, federation_id: FederationId) -> AdminResult<Amount> {
    let client = gateway.select_client(federation_id).await?.into_value();
    client.get_balance_for_btc().await
}

async fn mint_counts(
    gateway: &Gateway,
    federation_id: FederationId,
) -> AdminResult<BTreeMap<Denomination, u64>> {
    let client = gateway.select_client(federation_id).await?.into_value();
    let mint = client
        .get_first_module::<MintV2ClientModule>()
        .expect("MintV2 module is always attached to gateway clients");
    Ok(mint.get_count_by_denomination().await)
}

async fn wallet_send_fee_for(
    gateway: &Gateway,
    federation_id: FederationId,
) -> AdminResult<bitcoin::Amount> {
    let client = gateway.select_client(federation_id).await?.into_value();
    let wallet = client.get_first_module::<fedimint_walletv2_client::WalletClientModule>()?;
    wallet
        .send_fee()
        .await
        .map_err(|err| anyhow::anyhow!("{err:?}"))
}
