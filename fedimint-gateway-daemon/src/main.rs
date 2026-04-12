#![warn(missing_docs)]
//! This crate provides `gatewayd`, the Fedimint gateway binary.
//!
//! The binary contains logic for sending/receiving Lightning payments on behalf
//! of Fedimint clients in one or more connected Federations.
//!
//! It runs a webserver with a REST API that can be used by Fedimint
//! clients to request routing of payments through the Lightning Network.
//! The API also has endpoints for managing the gateway.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use bitcoin::Network;
use bitcoin::hashes::Hash;
use clap::{ArgGroup, Parser};
use fedimint_bip39::{Bip39RootSecretStrategy, Language, Mnemonic};
use fedimint_client::Client;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_core::db::Database;
use fedimint_core::envs::is_env_var_set;
use fedimint_core::fedimint_build_code_version_env;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::task::block_in_place;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow, SafeUrl};
use fedimint_gateway_common::{InterceptPaymentRequest, PaymentFee};
use fedimint_gateway_daemon::client::GatewayClientBuilder;
use fedimint_gateway_daemon::{AppState, DB_FILE, LDK_NODE_DB_FOLDER, cli};
use fedimint_gwv2_client::GatewayClientModuleV2;
use fedimint_logging::{LOG_GATEWAY, LOG_LIGHTNING, TracingSetup};
use fedimint_mintv2_client::MintClientInit;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeAlias;
use lightning::types::payment::PaymentHash;
use rand::rngs::OsRng;
#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;
use tracing::{debug, info, warn};

#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
#[global_allocator]
// rocksdb suffers from memory fragmentation when using standard allocator
static GLOBAL: Jemalloc = Jemalloc;

/// Command line parameters for starting the gateway.
#[derive(Parser)]
#[command(version)]
#[command(
    group(
        ArgGroup::new("bitcoind_password_auth")
           .args(["bitcoind_password"])
           .multiple(false)
    ),
    group(
        ArgGroup::new("bitcoind_auth")
            .args(["bitcoind_url"])
            .requires("bitcoind_password_auth")
            .requires_all(["bitcoind_username", "bitcoind_url"])
    ),
    group(
        ArgGroup::new("bitcoin_rpc")
            .required(true)
            .multiple(true)
            .args(["bitcoind_url", "esplora_url"])
    )
)]
pub struct GatewayOpts {
    /// Path to folder containing gateway config and data files
    #[arg(long = "data-dir", env = "FM_GATEWAY_DATA_DIR")]
    pub data_dir: PathBuf,

    /// Public API listen address
    #[arg(
        long = "api-bind",
        env = "FM_GATEWAY_API_BIND",
        default_value = "0.0.0.0:8175"
    )]
    pub api_bind: SocketAddr,

    /// CLI/admin listen address
    #[arg(
        long = "cli-bind",
        env = "FM_GATEWAY_CLI_BIND",
        default_value = "127.0.0.1:8176"
    )]
    pub cli_bind: SocketAddr,

    /// Public URL from which the webserver API is reachable
    #[arg(long = "api-addr", env = "FM_GATEWAY_API_ADDR")]
    pub api_addr: Option<SafeUrl>,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = "FM_GATEWAY_NETWORK")]
    pub network: Network,

    /// The username to use when connecting to bitcoind
    #[arg(long, env = "FM_BITCOIND_USERNAME")]
    pub bitcoind_username: Option<String>,

    /// The password to use when connecting to bitcoind
    #[arg(long, env = "FM_BITCOIND_PASSWORD")]
    pub bitcoind_password: Option<String>,

    /// Bitcoind RPC URL, e.g. <http://127.0.0.1:8332>
    #[arg(long, env = "FM_BITCOIND_URL")]
    pub bitcoind_url: Option<SafeUrl>,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = "FM_ESPLORA_URL")]
    pub esplora_url: Option<SafeUrl>,

    /// LDK lightning node listen port
    #[arg(
        long = "ldk-lightning-port",
        env = "FM_PORT_LDK",
        default_value_t = 9735
    )]
    pub lightning_port: u16,

    /// LDK node alias
    #[arg(long = "ldk-alias", env = "FM_LDK_ALIAS", default_value = "")]
    pub ldk_alias: String,

    /// The default routing fees that are applied to new federations
    #[arg(long = "default-routing-fees", env = "FM_DEFAULT_ROUTING_FEES", default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_routing_fees: PaymentFee,

    /// The default transaction fees that are applied to new federations
    #[arg(long = "default-transaction-fees", env = "FM_DEFAULT_TRANSACTION_FEES", default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_transaction_fees: PaymentFee,
}

fn main() -> anyhow::Result<()> {
    let runtime = Arc::new(tokio::runtime::Runtime::new()?);
    runtime.block_on(async {
        fedimint_core::util::handle_version_hash_command(fedimint_build_code_version_env!());
        TracingSetup::default().init()?;

        // 1. Parse CLI args
        let opts = GatewayOpts::parse();

        // 2. Open database
        let gateway_db = Database::new(
            fedimint_rocksdb::RocksDb::build(opts.data_dir.join(DB_FILE)).open().await?,
            ModuleDecoderRegistry::default(),
        );

        // 3. Load or generate mnemonic
        if AppState::load_mnemonic(&gateway_db).await.is_none() {
            let mnemonic = if let Ok(words) = std::env::var("FM_GATEWAY_MNEMONIC") {
                info!(target: LOG_GATEWAY, "Using provided mnemonic from environment variable");
                Mnemonic::parse_in_normalized(Language::English, words.as_str())
                    .map_err(|e| anyhow!("Seed phrase provided in environment was invalid {e:?}"))?
            } else {
                debug!(target: LOG_GATEWAY, "Generating mnemonic and writing entropy to client storage");
                Bip39RootSecretStrategy::<12>::random(&mut OsRng)
            };

            Client::store_encodable_client_secret(&gateway_db, mnemonic.to_entropy())
                .await
                .map_err(|e| anyhow!("Mnemonic error: {e}"))?;
        }

        let mnemonic = AppState::load_mnemonic(&gateway_db)
            .await
            .expect("mnemonic should be set");

        // 4. Build client module registry
        let mut registry = ClientModuleInitRegistry::new();
        registry.attach(MintClientInit);
        registry.attach(fedimint_walletv2_client::WalletClientInit);

        let client_builder = GatewayClientBuilder::new(opts.data_dir.clone(), registry).await?;

        // 5. Build LDK node
        let alias = if opts.ldk_alias.is_empty() {
            "LDK Gateway".to_string()
        } else {
            opts.ldk_alias.clone()
        };
        let mut alias_bytes = [0u8; 32];
        let truncated = &alias.as_bytes()[..alias.len().min(32)];
        alias_bytes[..truncated.len()].copy_from_slice(truncated);

        let mut node_builder = ldk_node::Builder::from_config(ldk_node::config::Config {
            network: opts.network,
            listening_addresses: Some(vec![SocketAddress::TcpIpV4 {
                addr: [0, 0, 0, 0],
                port: opts.lightning_port,
            }]),
            node_alias: Some(NodeAlias(alias_bytes)),
            ..Default::default()
        });

        node_builder.set_entropy_bip39_mnemonic(mnemonic, None);

        match (opts.bitcoind_url.clone(), opts.esplora_url.clone()) {
            (Some(url), _) => {
                node_builder.set_chain_source_bitcoind_rpc(
                    url.host_str()
                        .expect("Missing bitcoind host")
                        .to_string(),
                    url.port().expect("Missing bitcoind port"),
                    opts.bitcoind_username
                        .clone()
                        .expect("FM_BITCOIND_USERNAME is required"),
                    opts.bitcoind_password
                        .clone()
                        .expect("FM_BITCOIND_PASSWORD is required"),
                );
            }
            (None, Some(url)) => {
                node_builder.set_chain_source_esplora(url.to_string(), None);
            }
            _ => unreachable!("ArgGroup enforces at least one chain source"),
        }

        let ldk_data_dir = client_builder.data_dir().join(LDK_NODE_DB_FOLDER);
        node_builder.set_storage_dir_path(
            ldk_data_dir
                .to_str()
                .expect("Invalid data dir path")
                .to_string(),
        );

        info!(target: LOG_LIGHTNING, data_dir = %ldk_data_dir.display(), %alias, "Starting LDK Node...");

        let node = node_builder.build()?;

        let ldk_runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build LDK runtime"),
        );

        node.start_with_runtime(ldk_runtime)
            .map_err(|err| anyhow!("Failed to start LDK Node: {err}"))?;

        info!("Successfully started LDK Node");
        let node = Arc::new(node);

        // 6. Wait for chain sync
        install_crypto_provider().await;
        if !is_env_var_set("FM_GATEWAY_SKIP_WAIT_FOR_SYNC") {
            wait_for_chain_sync(&node).await?;
        }

        // 7. Construct AppState
        info!(
            target: LOG_GATEWAY,
            version = %fedimint_build_code_version_env!(),
            "Starting gatewayd",
        );

        let state = AppState::new(
            gateway_db,
            client_builder,
            node.clone(),
            opts.api_bind,
            opts.cli_bind,
            opts.api_addr,
            opts.network,
            opts.default_routing_fees,
            opts.default_transaction_fees,
        )
        .await?;

        // 8. Load federation clients
        state.load_clients().await?;

        // 9. Create task group for graceful shutdown
        let task_group = fedimint_core::task::TaskGroup::new();
        task_group.install_kill_handler();

        // 10. Spawn tasks
        let public_task = runtime.spawn(fedimint_gateway_daemon::public::run_public(
            state.clone(),
            task_group.make_handle(),
        ));

        let cli_task = runtime.spawn(cli::run_cli(state.clone(), task_group.make_handle()));

        let events_task = runtime.spawn(process_ldk_events(state.clone(), task_group.make_handle()));

        // 11. Wait for shutdown signal
        shutdown_signal().await;

        info!(target: LOG_GATEWAY, "Gatewayd shutting down...");
        task_group.shutdown_join_all(None).await?;

        if let Err(e) = public_task.await {
            warn!(target: LOG_GATEWAY, err = %e, "Failed to join public webserver task");
        }
        if let Err(e) = cli_task.await {
            warn!(target: LOG_GATEWAY, err = %e, "Failed to join CLI webserver task");
        }
        if let Err(e) = events_task.await {
            warn!(target: LOG_GATEWAY, err = %e, "Failed to join LDK events task");
        }

        info!(target: LOG_GATEWAY, "Gatewayd exiting...");
        Ok(())
    })
}

async fn shutdown_signal() {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("Failed to install SIGTERM handler")
        .recv()
        .await;
}

async fn wait_for_chain_sync(node: &ldk_node::Node) -> anyhow::Result<()> {
    use fedimint_core::envs::{FM_IN_DEVIMINT_ENV, is_env_var_set};
    use fedimint_core::util::{backoff_util, retry};

    if is_env_var_set(FM_IN_DEVIMINT_ENV) {
        block_in_place(|| {
            let _ = node.sync_wallets();
        });
    }

    retry(
        "Wait for chain sync",
        backoff_util::background_backoff(),
        || async {
            let node_status = node.status();
            let block_height = node_status.current_best_block.height;
            if node_status.latest_lightning_wallet_sync_timestamp.is_some() {
                Ok(())
            } else {
                warn!(
                    target: LOG_LIGHTNING,
                    block_height = %block_height,
                    "Lightning node is not synced yet",
                );
                Err(anyhow::anyhow!("Not synced yet"))
            }
        },
    )
    .await?;

    info!(target: LOG_LIGHTNING, "Gateway successfully synced with the chain");
    Ok(())
}

// ---------------------------------------------------------------------------
// LDK event loop
// ---------------------------------------------------------------------------

async fn process_ldk_events(state: AppState, handle: fedimint_core::task::TaskHandle) {
    loop {
        let event = tokio::select! {
            event = state.node.next_event_async() => event,
            () = handle.make_shutdown_rx() => break,
        };

        process_ldk_event(&state, event).await;

        if let Err(e) = state.node.event_handled() {
            warn!(
                target: LOG_LIGHTNING,
                err = %e.fmt_compact(),
                "Failed to mark event handled",
            );
        }
    }
}

async fn process_ldk_event(state: &AppState, event: ldk_node::Event) {
    match event {
        ldk_node::Event::PaymentClaimable {
            payment_id: _,
            payment_hash,
            claimable_amount_msat,
            claim_deadline,
            custom_records: _,
        } => {
            let hash = Hash::from_slice(&payment_hash.0).expect("Failed to create Hash");

            let payment_request = InterceptPaymentRequest {
                payment_hash: hash,
                amount_msat: claimable_amount_msat,
                expiry: claim_deadline.unwrap_or_default(),
                short_channel_id: None,
                incoming_chan_id: 0,
                htlc_id: 0,
            };

            handle_lightning_payment(state, payment_request).await;
        }
        ldk_node::Event::ChannelPending {
            channel_id,
            user_channel_id,
            former_temporary_channel_id: _,
            counterparty_node_id: _,
            funding_txo,
        } => {
            info!(
                target: LOG_LIGHTNING,
                %channel_id,
                "LDK Channel is pending",
            );
            let mut channels = state.pending_channels.write().await;
            if let Some(sender) =
                channels.remove(&fedimint_gateway_daemon::UserChannelId(user_channel_id))
            {
                let _ = sender.send(Ok(funding_txo));
            } else {
                debug!(
                    ?user_channel_id,
                    "No channel pending channel open for user channel id",
                );
            }
        }
        ldk_node::Event::ChannelClosed {
            channel_id,
            user_channel_id,
            counterparty_node_id: _,
            reason,
        } => {
            info!(
                target: LOG_LIGHTNING,
                %channel_id,
                "LDK Channel is closed",
            );
            let mut channels = state.pending_channels.write().await;
            if let Some(sender) =
                channels.remove(&fedimint_gateway_daemon::UserChannelId(user_channel_id))
            {
                let reason = if let Some(reason) = reason {
                    reason.to_string()
                } else {
                    "Channel has been closed".to_string()
                };
                let _ = sender.send(Err(anyhow::anyhow!(reason)));
            } else {
                debug!(
                    ?user_channel_id,
                    "No channel pending channel open for user channel id",
                );
            }
        }
        _ => {}
    }
}

/// Handles an intercepted lightning payment. If the payment is part of an
/// incoming payment to a federation, spawns a state machine and hands the
/// payment off to it. Otherwise, fails the HTLC since forwarding is not
/// supported.
async fn handle_lightning_payment(state: &AppState, payment_request: InterceptPaymentRequest) {
    info!(
        target: LOG_GATEWAY,
        payment_hash = %payment_request.payment_hash,
        amount_msat = %payment_request.amount_msat,
        "Intercepting lightning payment",
    );

    if try_handle_lightning_payment_lnv2(state, &payment_request)
        .await
        .is_ok()
    {
        return;
    }

    // No matching federation contract; fail the HTLC
    let ph = PaymentHash(*payment_request.payment_hash.as_byte_array());
    if let Err(err) = state.node.bolt11_payment().fail_for_hash(ph) {
        warn!(
            target: LOG_GATEWAY,
            err = %err.fmt_compact(),
            "Error failing unmatched HTLC",
        );
    }
}

/// Tries to handle a lightning payment using the LNv2 protocol.
/// Returns `Ok` if the payment was handled, `Err` otherwise.
async fn try_handle_lightning_payment_lnv2(
    state: &AppState,
    htlc_request: &InterceptPaymentRequest,
) -> fedimint_gateway_daemon::Result<()> {
    use fedimint_lnv2_common::contracts::PaymentImage;

    let (contract, client) = state
        .get_registered_incoming_contract_and_client_v2(
            PaymentImage::Hash(htlc_request.payment_hash),
            htlc_request.amount_msat,
        )
        .await?;

    if let Err(err) = client
        .get_first_module::<GatewayClientModuleV2>()
        .expect("Must have client module")
        .relay_incoming_htlc(
            htlc_request.payment_hash,
            htlc_request.incoming_chan_id,
            htlc_request.htlc_id,
            contract,
            htlc_request.amount_msat,
        )
        .await
    {
        warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Error relaying incoming lightning payment");

        let ph = PaymentHash(*htlc_request.payment_hash.as_byte_array());
        if let Err(err) = state.node.bolt11_payment().fail_for_hash(ph) {
            warn!(
                target: LOG_GATEWAY,
                err = %err.fmt_compact(),
                "Error failing HTLC after relay error",
            );
        }
    }

    Ok(())
}
