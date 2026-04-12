#![warn(missing_docs)]
//! This crate provides `gatewayd`, the Fedimint gateway binary.
//!
//! The binary contains logic for sending/receiving Lightning payments on behalf
//! of Fedimint clients in one or more connected Federations.
//!
//! It runs a webserver with a REST API that can be used by Fedimint
//! clients to request routing of payments through the Lightning Network.
//! The API also has endpoints for managing the gateway.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use bitcoin::Network;
use bitcoin::hashes::{Hash as _, sha256};
use clap::{ArgGroup, Parser};
use fedimint_bip39::Bip39RootSecretStrategy;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_core::db::Database;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow, SafeUrl};
use fedimint_core::{Amount, fedimint_build_code_version_env};
use fedimint_gateway_common::PaymentFee;
use fedimint_gateway_daemon::client::GatewayClientFactory;
use fedimint_gateway_daemon::{
    AppState, DB_FILE, LDK_NODE_DB_FOLDER, cli, derive_gateway_keypair, public,
};
use fedimint_gwv2_client::GatewayClientModuleV2;
use fedimint_logging::{LOG_GATEWAY, LOG_LIGHTNING, TracingSetup};
use fedimint_mintv2_client::MintClientInit;
use fedimint_rocksdb::RocksDb;
use fedimint_walletv2_client::WalletClientInit;
use lightning::types::payment::PaymentHash;
use rand::rngs::OsRng;
#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;
use tokio::sync::RwLock;
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

    /// Network address and port for the lightning P2P interface
    #[arg(long = "ldk-bind", env = "FM_LDK_BIND", default_value = "0.0.0.0:8177")]
    pub ldk_bind: SocketAddr,

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

    /// Base routing fee in millisatoshis for Lightning payments
    #[arg(long, env = "FM_ROUTING_FEE_BASE_MSAT", default_value_t = 2000)]
    pub routing_fee_base_msat: u64,

    /// Routing fee rate in parts per million for Lightning payments
    #[arg(long, env = "FM_ROUTING_FEE_PPM", default_value_t = 3000)]
    pub routing_fee_ppm: u64,

    /// Base transaction fee in millisatoshis for federation transactions
    #[arg(long, env = "FM_TRANSACTION_FEE_BASE_MSAT", default_value_t = 2000)]
    pub transaction_fee_base_msat: u64,

    /// Transaction fee rate in parts per million for federation transactions
    #[arg(long, env = "FM_TRANSACTION_FEE_PPM", default_value_t = 3000)]
    pub transaction_fee_ppm: u64,
}

fn main() -> anyhow::Result<()> {
    let runtime = Arc::new(tokio::runtime::Runtime::new()?);
    runtime.block_on(async {
        fedimint_core::util::handle_version_hash_command(fedimint_build_code_version_env!());
        TracingSetup::default().init()?;

        // 1. Parse CLI args
        let opts = GatewayOpts::parse();

        // 2. Open database
        install_crypto_provider().await;

        let gateway_db = Database::new(
            RocksDb::build(opts.data_dir.join(DB_FILE)).open().await?,
            ModuleDecoderRegistry::default(),
        );

        // 3. Load or init client factory (mnemonic)
        let mut registry = ClientModuleInitRegistry::new();
        registry.attach(MintClientInit);
        registry.attach(WalletClientInit);

        let client_factory =
            match GatewayClientFactory::try_load(gateway_db.clone(), registry.clone()).await? {
                Some(factory) => factory,
                None => {
                    GatewayClientFactory::init(
                        gateway_db.clone(),
                        Bip39RootSecretStrategy::<12>::random(&mut OsRng),
                        registry,
                    )
                    .await?
                }
            };

        let mnemonic = client_factory.mnemonic().clone();
        let gateway_keypair = derive_gateway_keypair(&mnemonic);

        // 4. Build LDK node
        let ldk_data_dir = opts
            .data_dir
            .join(LDK_NODE_DB_FOLDER)
            .to_str()
            .expect("Invalid data dir path")
            .to_string();

        let mut node_builder = ldk_node::Builder::new();

        node_builder.set_network(opts.network);
        node_builder.set_node_alias("fedimint-gateway-daemon".to_string())?;
        node_builder.set_listening_addresses(vec![opts.ldk_bind.into()])?;
        node_builder.set_entropy_bip39_mnemonic(mnemonic, None);
        node_builder.set_storage_dir_path(ldk_data_dir);

        match (opts.bitcoind_url.clone(), opts.esplora_url.clone()) {
            (Some(url), _) => {
                node_builder.set_chain_source_bitcoind_rpc(
                    url.host_str().expect("Missing bitcoind host").to_string(),
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

        info!(target: LOG_LIGHTNING, "Starting LDK Node...");

        let node = Arc::new(node_builder.build()?);

        let ldk_runtime = Arc::new(tokio::runtime::Runtime::new()?);

        node.start_with_runtime(ldk_runtime.clone())?;

        info!("Successfully started LDK Node");

        // 5. Construct AppState
        let state = AppState {
            clients: Arc::new(RwLock::new(BTreeMap::new())),
            node: node.clone(),
            client_factory,
            gateway_db,
            api_bind: opts.api_bind,
            cli_bind: opts.cli_bind,
            network: opts.network,
            routing_fees: PaymentFee {
                base: Amount::from_msats(opts.routing_fee_base_msat),
                parts_per_million: opts.routing_fee_ppm,
            },
            transaction_fees: PaymentFee {
                base: Amount::from_msats(opts.transaction_fee_base_msat),
                parts_per_million: opts.transaction_fee_ppm,
            },
            gateway_keypair,
            api_addr: opts.api_addr,
            outbound_lightning_payment_lock_pool: Arc::new(lockable::LockPool::new()),
        };

        // 8. Load federation clients
        state.load_clients().await?;

        // 9. Create task group for graceful shutdown
        let task_group = fedimint_core::task::TaskGroup::new();

        // 10. Spawn tasks
        let public_task =
            runtime.spawn(public::run_public(state.clone(), task_group.make_handle()));

        let cli_task = runtime.spawn(cli::run_cli(state.clone(), task_group.make_handle()));

        let events_task =
            runtime.spawn(process_ldk_events(state.clone(), task_group.make_handle()));

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
            payment_hash,
            claimable_amount_msat,
            ..
        } => {
            handle_lightning_payment(state, payment_hash.0, claimable_amount_msat).await;
        }
        _ => {}
    }
}

/// Handles an intercepted lightning payment. If the payment is part of an
/// incoming payment to a federation, spawns a state machine and hands the
/// payment off to it. Otherwise, fails the HTLC since forwarding is not
/// supported.
async fn handle_lightning_payment(state: &AppState, payment_hash: [u8; 32], amount_msat: u64) {
    if try_handle_lightning_payment_lnv2(state, payment_hash, amount_msat)
        .await
        .is_ok()
    {
        return;
    }

    if let Err(err) = state
        .node
        .bolt11_payment()
        .fail_for_hash(PaymentHash(payment_hash))
    {
        warn!(
            target: LOG_GATEWAY,
            err = %err.fmt_compact(),
            "Error failing unmatched HTLC",
        );
    }
}

async fn try_handle_lightning_payment_lnv2(
    state: &AppState,
    payment_hash: [u8; 32],
    amount_msat: u64,
) -> anyhow::Result<()> {
    use fedimint_lnv2_common::contracts::PaymentImage;

    let hash = sha256::Hash::from_byte_array(payment_hash);

    let (contract, client) = state
        .get_registered_incoming_contract_and_client_v2(PaymentImage::Hash(hash), amount_msat)
        .await?;

    if let Err(err) = client
        .get_first_module::<GatewayClientModuleV2>()
        .expect("Must have client module")
        .relay_incoming_htlc(hash, 0, 0, contract, amount_msat)
        .await
    {
        warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Error relaying incoming lightning payment");

        if let Err(err) = state
            .node
            .bolt11_payment()
            .fail_for_hash(PaymentHash(payment_hash))
        {
            warn!(
                target: LOG_GATEWAY,
                err = %err.fmt_compact(),
                "Error failing HTLC after relay error",
            );
        }
    }

    Ok(())
}
