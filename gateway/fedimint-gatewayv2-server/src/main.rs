#![warn(missing_docs)]
//! This crate provides `gatewaydv2`, a Fedimint gateway binary scoped to LDK
//! and LNv2.
//!
//! It started as a copy of the `gatewayd` binary and its
//! `fedimint-gateway-server` crate. LND and LNv1 support are being stripped out
//! so that `gatewaydv2` only supports the LDK lightning backend and the LNv2
//! module.
//!
//! Like `gatewayd`, it runs a webserver with a REST API that Fedimint clients
//! use to request routing of payments through the Lightning Network, plus an
//! admin CLI over a local Unix socket. The daemon lifecycle is modeled on the
//! sibling picomint gateway: `main` wires every component and fires each
//! long-running task off onto the runtime, then blocks on `SIGTERM`; the
//! runtime drop on exit aborts the tasks.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::anyhow;
use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_bip39::Mnemonic;
use fedimint_core::envs::is_running_in_test_env;
use fedimint_core::fedimint_build_code_version_env;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::util::{FmtCompact as _, SafeUrl, handle_version_hash_command};
use fedimint_gatewayv2_server::analytics::Analytics;
use fedimint_gatewayv2_server::cli::run_cli;
use fedimint_gatewayv2_server::client::GatewayClientFactory;
use fedimint_gatewayv2_server::public::run_public;
use fedimint_gatewayv2_server::{AppState, LDK_NODE_DB_FOLDER};
use fedimint_lnv2_common::gateway_api::PaymentFee;
use fedimint_logging::{LOG_GATEWAY, LOG_LIGHTNING_LDK, TracingSetup};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeAlias;
use ldk_node::logger::{LogLevel, LogRecord, LogWriter};
#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
#[global_allocator]
// rocksdb suffers from memory fragmentation when using standard allocator
static GLOBAL: Jemalloc = Jemalloc;

/// Command line parameters for starting the gateway. `gatewaydv2` is LDK + LNv2
/// only, so there is no lightning-backend subcommand.
#[derive(Parser)]
#[command(version)]
#[command(group(
    ArgGroup::new("bitcoin_rpc")
        .required(true)
        .multiple(true)
        .args(["bitcoind_url", "esplora_url"])
))]
pub struct GatewayOpts {
    /// Path to folder containing gateway config and data files
    #[arg(long = "data-dir", env = "FM_DATA_DIR")]
    pub data_dir: PathBuf,

    /// Address the gateway's API webserver (the LNv2 routes) listens on.
    #[arg(long = "api-addr", env = "FM_API_ADDR", default_value = "0.0.0.0:8080")]
    pub api_addr: SocketAddr,

    /// Address and port for the LDK node's lightning P2P (BOLT) interface.
    #[arg(long = "ldk-addr", env = "FM_LDK_ADDR", default_value = "0.0.0.0:9735")]
    pub ldk_addr: SocketAddr,

    /// The LDK node's advertised alias.
    #[arg(long = "ldk-alias", env = "FM_LDK_ALIAS", default_value = "")]
    pub ldk_alias: String,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = "FM_NETWORK", default_value = "bitcoin")]
    pub network: Network,

    /// Bitcoind RPC URL with credentials embedded in the URL, e.g.
    /// `http://user:pass@127.0.0.1:8332`.
    #[arg(long, env = "FM_BITCOIND_URL")]
    pub bitcoind_url: Option<SafeUrl>,

    /// Esplora HTTP base URL, e.g. <https://blockstream.info/api>
    #[arg(long, env = "FM_ESPLORA_URL")]
    pub esplora_url: Option<SafeUrl>,

    /// The default routing fees that are applied to new federations
    #[arg(long = "default-routing-fees", env = "FM_DEFAULT_ROUTING_FEES", default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_routing_fees: PaymentFee,

    /// The default transaction fees that are applied to new federations
    #[arg(long = "default-transaction-fees", env = "FM_DEFAULT_TRANSACTION_FEES", default_value_t = PaymentFee::TRANSACTION_FEE_DEFAULT)]
    pub default_transaction_fees: PaymentFee,
}

fn main() -> anyhow::Result<()> {
    handle_version_hash_command(fedimint_build_code_version_env!());
    TracingSetup::default().init()?;

    // 1. Parse CLI args and create the runtime.
    let opts = GatewayOpts::parse();
    let runtime = Arc::new(tokio::runtime::Runtime::new()?);

    info!(target: LOG_GATEWAY, version = %fedimint_build_code_version_env!(), "Starting gatewaydv2");

    runtime.block_on(install_crypto_provider());

    // 2. Open the database, ensure the mnemonic, and build the client factory.
    let (gateway_db, client_factory, mnemonic) =
        runtime.block_on(GatewayClientFactory::prepare(&opts.data_dir))?;

    // 3. Build and start the LDK node. Passing the runtime here is what lets the
    //    node be `Arc<Node>` on `AppState` rather than lazily created later.
    let node = build_ldk_node(&opts, mnemonic, runtime.clone())?;

    // 4. Assemble the shared gateway state. The analytics SQLite mirror is derived
    //    state and starts fresh on every boot.
    let analytics = Analytics::wipe_and_init(&opts.data_dir)?;

    let state = AppState {
        clients: Arc::new(RwLock::new(BTreeMap::new())),
        node,
        client_factory,
        gateway_db,
        data_dir: opts.data_dir,
        api_addr: opts.api_addr,
        network: opts.network,
        default_routing_fees: opts.default_routing_fees,
        default_transaction_fees: opts.default_transaction_fees,
        analytics,
    };

    // 5. Fire-and-forget every long-running task. Federation clients are
    //    lazy-loaded on first use, each spawning its own receive trailer when
    //    built; all work is persisted incrementally and idempotent on retry, so the
    //    runtime drop on process exit aborts cleanly.
    runtime.spawn(state.clone().process_ldk_events());
    runtime.spawn(run_cli(state.clone()));
    runtime.spawn(run_public(state));

    // 6. Block main on SIGTERM so the runtime stays alive; on signal, return and
    //    let the runtime drop abort all tasks.
    runtime.block_on(shutdown_signal());

    info!(target: LOG_GATEWAY, "Gatewaydv2 exiting...");

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("Failed to install SIGTERM handler")
        .recv()
        .await;
}

/// Builds and starts the LDK node from the gateway's configuration and returns
/// it. Called from `main` before the [`AppState`] is assembled; the gateway's
/// lightning bookkeeping (the pay lock-pool and pending-channels map) lives on
/// [`AppState`] itself.
fn build_ldk_node(
    opts: &GatewayOpts,
    mnemonic: Mnemonic,
    runtime: Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<Arc<ldk_node::Node>> {
    let mut bytes = [0u8; 32];
    let alias = if opts.ldk_alias.is_empty() {
        "LDK Gateway".to_string()
    } else {
        opts.ldk_alias.clone()
    };
    let alias_bytes = alias.as_bytes();
    let truncated = &alias_bytes[..alias_bytes.len().min(32)];
    bytes[..truncated.len()].copy_from_slice(truncated);
    let node_alias = Some(NodeAlias(bytes));

    let listening_address = match opts.ldk_addr {
        SocketAddr::V4(addr) => SocketAddress::TcpIpV4 {
            addr: addr.ip().octets(),
            port: addr.port(),
        },
        SocketAddr::V6(addr) => SocketAddress::TcpIpV6 {
            addr: addr.ip().octets(),
            port: addr.port(),
        },
    };

    let mut node_builder = ldk_node::Builder::from_config(ldk_node::config::Config {
        network: opts.network,
        listening_addresses: Some(vec![listening_address]),
        node_alias,
        ..Default::default()
    });

    // Route LDK's logs into the gateway's `tracing` subscriber so they land in
    // the same place (stderr / log file) and honor `RUST_LOG`, instead of LDK's
    // default append-only `ldk_node/ldk_node.log` file.
    node_builder.set_custom_logger(Arc::new(LdkTracingLogger {
        in_test_env: is_running_in_test_env(),
    }));

    node_builder.set_entropy_bip39_mnemonic(mnemonic, None);

    // LDK uses bitcoind purely as a chain-data source via node-level RPCs, with
    // credentials embedded in the URL (`http://user:pass@host:port`), so —
    // unlike v1 gatewayd — no watch-only wallet is created.
    match (opts.bitcoind_url.as_ref(), opts.esplora_url.as_ref()) {
        // Use bitcoind by default if both are set
        (Some(url), _) => {
            node_builder.set_chain_source_bitcoind_rpc(
                url.host_str()
                    .expect("Could not retrieve host from bitcoind RPC url")
                    .to_string(),
                url.port()
                    .expect("Could not retrieve port from bitcoind RPC url"),
                url.username().to_string(),
                url.password().unwrap_or_default().to_string(),
            );
        }
        (None, Some(url)) => {
            node_builder.set_chain_source_esplora(get_esplora_url(url)?, None);
        }
        _ => unreachable!("ArgGroup already enforced XOR relation"),
    }

    let data_dir = opts.data_dir.join(LDK_NODE_DB_FOLDER);
    let Some(data_dir_str) = data_dir.to_str() else {
        return Err(anyhow!("Invalid data dir path"));
    };
    node_builder.set_storage_dir_path(data_dir_str.to_string());

    info!(data_dir = %data_dir_str, alias = %alias, "Starting LDK Node...");
    let node = Arc::new(node_builder.build()?);
    node.start_with_runtime(runtime)
        .map_err(|err| anyhow!("Failed to start LDK Node: {}", err.fmt_compact()))?;

    info!("Successfully started LDK Gateway");
    Ok(node)
}

/// When a port is specified in the Esplora URL, the esplora client inside LDK
/// node cannot connect to the lightning node when there is a trailing slash.
/// The `SafeUrl::Display` function will always serialize the `SafeUrl` with a
/// trailing slash, which causes the connection to fail.
///
/// To handle this, we explicitly construct the esplora URL when a port is
/// specified.
fn get_esplora_url(server_url: &SafeUrl) -> anyhow::Result<String> {
    // Esplora client cannot handle trailing slashes
    let host = server_url
        .host_str()
        .ok_or(anyhow!("Missing esplora host"))?;
    let server_url = if let Some(port) = server_url.port() {
        format!("{}://{}:{}", server_url.scheme(), host, port)
    } else {
        server_url.to_string()
    };
    Ok(server_url)
}

/// Forwards `ldk-node`'s log records into the gateway's `tracing` subscriber.
///
/// By default `ldk-node` writes to its own append-only `ldk_node/ldk_node.log`
/// file, which is invisible to stdout/stderr log collectors and grows without
/// bound. Routing the records through `tracing` (under the
/// [`LOG_LIGHTNING_LDK`] target) puts them alongside the rest of gatewayd's
/// logs and makes them filterable via `RUST_LOG`.
struct LdkTracingLogger {
    /// Whether we're running under devimint/tests. When set, some benign LDK
    /// error logs that are expected in regtest are downgraded to avoid spamming
    /// the test output. See [`Self::downgraded_level`].
    in_test_env: bool,
}

impl LdkTracingLogger {
    /// Returns the level to emit `record` at, downgrading benign-but-noisy LDK
    /// errors when running under devimint/tests.
    ///
    /// In regtest there is no fee-rate history, so `ldk-node` logs "Failed to
    /// retrieve fee rate estimates ... Falling back to default" at `Error` on
    /// essentially every sync. This is harmless (LDK falls back to a default
    /// feerate), so in test environments we emit it at `Debug` instead. In
    /// production the original `Error` level is preserved, since a persistent
    /// failure there can indicate a real problem.
    fn downgraded_level(&self, record: &LogRecord<'_>) -> LogLevel {
        if self.in_test_env
            && record.level == LogLevel::Error
            && record.module_path == "ldk_node::chain"
            && format!("{}", record.args).contains("Failed to retrieve fee rate estimates")
        {
            LogLevel::Debug
        } else {
            record.level
        }
    }
}

impl LogWriter for LdkTracingLogger {
    fn log(&self, record: LogRecord<'_>) {
        // `tracing` requires a static level per call-site, so match each LDK level.
        match self.downgraded_level(&record) {
            LogLevel::Gossip | LogLevel::Trace => tracing::trace!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Debug => debug!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Info => info!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Warn => warn!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
            LogLevel::Error => error!(
                target: LOG_LIGHTNING_LDK,
                ldk_module = record.module_path, line = record.line, "{}", record.args,
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::util::SafeUrl;

    use super::get_esplora_url;

    #[test]
    fn verify_ldk_esplora_url() {
        let url = SafeUrl::parse("https://mempool.space/api/").expect("Cannot parse URL");
        let esplora_url = get_esplora_url(&url).expect("Could not get esplora URL");
        // URLs without ports are allowed to have trailing slashes
        assert!(esplora_url.ends_with('/'));

        let url = SafeUrl::parse("https://mutinynet.com/api/").expect("Cannot parse URL");
        let esplora_url = get_esplora_url(&url).expect("Could not get esplora URL");
        // URLs without ports are allowed to have trailing slashes
        assert!(esplora_url.ends_with('/'));

        let url = SafeUrl::parse("http://127.0.0.1:3003/").expect("Cannot parse URL");
        let esplora_url = get_esplora_url(&url).expect("Could not get esplora URL");
        // URLs with ports are NOT allowed to have trailing slashes
        assert!(!esplora_url.ends_with('/'));
    }
}
