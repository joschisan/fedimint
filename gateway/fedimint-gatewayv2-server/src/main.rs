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

mod connect;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr as _;
use std::sync::Arc;

use anyhow::anyhow;
use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_bip39::Mnemonic;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::util::{FmtCompact as _, SafeUrl, handle_version_hash_command};
use fedimint_core::{Amount, fedimint_build_code_version_env};
use fedimint_gatewayv2_server::analytics::Analytics;
use fedimint_gatewayv2_server::cli::run_cli;
use fedimint_gatewayv2_server::client::GatewayClientFactory;
use fedimint_gatewayv2_server::public::run_public;
use fedimint_gatewayv2_server::{AppState, LDK_NODE_DB_FOLDER};
use fedimint_lnv2_common::gateway_api::PaymentFee;
use fedimint_logging::{LOG_GATEWAY, TracingSetup};
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::scoring::ProbabilisticScoringFeeParameters;
#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;
use tokio::sync::RwLock;
use tracing::{info, warn};

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
    #[arg(
        long = "ldk-alias",
        env = "FM_LDK_ALIAS",
        default_value = "fedimint-gatewayv2-daemon"
    )]
    pub ldk_alias: String,

    /// Bitcoin network this gateway will be running on
    #[arg(long = "network", env = "FM_NETWORK", default_value = "bitcoin")]
    pub network: Network,

    /// Bitcoind RPC URL with credentials embedded in the URL, e.g.
    /// `http://user:pass@127.0.0.1:8332`.
    #[arg(long, env = "FM_BITCOIND_URL")]
    pub bitcoind_url: Option<SafeUrl>,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = "FM_ESPLORA_URL")]
    pub esplora_url: Option<SafeUrl>,

    /// Base send fee in millisatoshis: the gateway's tx cut on outgoing
    /// payments.
    #[arg(long, env = "SEND_FEE_BASE_MSAT", default_value_t = 2000)]
    pub send_fee_base_msat: u64,

    /// Send fee rate in parts per million: the gateway's tx cut on outgoing
    /// payments.
    #[arg(long, env = "SEND_FEE_PPM", default_value_t = 0)]
    pub send_fee_ppm: u64,

    /// Base receive fee in millisatoshis: the gateway's tx cut on incoming
    /// payments.
    #[arg(long, env = "RECEIVE_FEE_BASE_MSAT", default_value_t = 2000)]
    pub receive_fee_base_msat: u64,

    /// Receive fee rate in parts per million: the gateway's tx cut on incoming
    /// payments.
    #[arg(long, env = "RECEIVE_FEE_PPM", default_value_t = 0)]
    pub receive_fee_ppm: u64,

    /// Base Lightning routing fee in millisatoshis. Enforced exactly as
    /// LDK's `max_total_routing_fee_msat` cap on external outgoing payments.
    #[arg(long, env = "ROUTING_FEE_BASE_MSAT", default_value_t = 10_000)]
    pub routing_fee_base_msat: u64,

    /// Lightning routing fee rate in parts per million.
    #[arg(long, env = "ROUTING_FEE_PPM", default_value_t = 3000)]
    pub routing_fee_ppm: u64,

    /// How many of the network's most-connected nodes the background prober
    /// cycles through to keep the pathfinding scorer warm.
    #[arg(long, env = "FM_PROBE_TOP_NODE_COUNT", default_value_t = 100)]
    pub probe_top_node_count: usize,

    /// Scorer penalty in millisatoshis applied to recently probed channels so
    /// successive probes explore different routes instead of reusing one.
    #[arg(long, env = "FM_PROBE_DIVERSITY_PENALTY_MSAT", default_value_t = 250)]
    pub probe_diversity_penalty_msat: u64,

    /// Maximum millisatoshis locked in in-flight probes at any one time; a
    /// probe through an offline hop can hold its HTLC until timeout, so this
    /// caps the outbound liquidity that probing may tie up.
    #[arg(long, env = "FM_PROBE_MAX_LOCKED_MSAT", default_value_t = 500_000_000)]
    pub probe_max_locked_msat: u64,

    /// Lower bound in millisatoshis for a probe's randomly chosen amount.
    #[arg(long, env = "FM_PROBE_MIN_AMOUNT_MSAT", default_value_t = 1_000_000)]
    pub probe_min_amount_msat: u64,

    /// Upper bound in millisatoshis for a probe's randomly chosen amount.
    /// Larger probes teach the scorer about liquidity in the range of larger
    /// payments, at the cost of locking more liquidity per in-flight probe.
    #[arg(long, env = "FM_PROBE_MAX_AMOUNT_MSAT", default_value_t = 100_000_000)]
    pub probe_max_amount_msat: u64,

    /// Factor applied to all of the pathfinding scorer's penalty knobs over
    /// LDK's defaults, weighting payment success probability against routing
    /// fees: each sat of routing fee counts as 1/factor of a sat when
    /// comparing routes, so reliable, probe-validated routes beat marginally
    /// cheaper ones. 1 restores LDK's default weighting.
    #[arg(long, env = "FM_SCORER_PENALTY_FACTOR", default_value_t = 4)]
    pub scorer_penalty_factor: u64,
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
        send_fee: PaymentFee {
            base: Amount::from_msats(opts.send_fee_base_msat),
            parts_per_million: opts.send_fee_ppm,
        },
        receive_fee: PaymentFee {
            base: Amount::from_msats(opts.receive_fee_base_msat),
            parts_per_million: opts.receive_fee_ppm,
        },
        routing_fee: PaymentFee {
            base: Amount::from_msats(opts.routing_fee_base_msat),
            parts_per_million: opts.routing_fee_ppm,
        },
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
    let mut node_builder = ldk_node::Builder::new();

    // Route LDK's logs through the `log` facade, which the tracing subscriber
    // captures — so LDK errors (e.g. route-finding failures) show up in the
    // gateway logs instead of a sidecar `ldk_node.log` file.
    node_builder.set_log_facade_logger();

    node_builder.set_network(opts.network);
    node_builder.set_node_alias(opts.ldk_alias.clone())?;
    node_builder.set_listening_addresses(vec![opts.ldk_addr.into()])?;
    node_builder.set_entropy_bip39_mnemonic(mnemonic, None);

    // Weight payment success probability more heavily against routing fees.
    // The router picks the path minimizing `real fees + scorer penalties`, so
    // scaling every penalty knob by one factor is equivalent to counting each
    // sat of routing fee as 1/factor of a sat, while preserving the ratios
    // between the penalties that upstream tuned against real payment data.
    let mut scoring_fee_params = ProbabilisticScoringFeeParameters::default();
    scoring_fee_params.base_penalty_msat *= opts.scorer_penalty_factor;
    scoring_fee_params.base_penalty_amount_multiplier_msat *= opts.scorer_penalty_factor;
    scoring_fee_params.liquidity_penalty_multiplier_msat *= opts.scorer_penalty_factor;
    scoring_fee_params.liquidity_penalty_amount_multiplier_msat *= opts.scorer_penalty_factor;
    scoring_fee_params.historical_liquidity_penalty_multiplier_msat *= opts.scorer_penalty_factor;
    scoring_fee_params.historical_liquidity_penalty_amount_multiplier_msat *=
        opts.scorer_penalty_factor;
    scoring_fee_params.anti_probing_penalty_msat *= opts.scorer_penalty_factor;
    node_builder.set_scoring_fee_params(scoring_fee_params);

    // The default peer-to-peer gossip sync takes hours to assemble a usable
    // network graph on a fresh node, and every routed payment fails with
    // `RouteNotFound` until it does — pull snapshots from the LDK project's
    // Rapid Gossip Sync server instead. There is no RGS server for regtest,
    // where the two-node devimint topology needs no gossip anyway.
    if opts.network == Network::Bitcoin {
        node_builder
            .set_gossip_source_rgs("https://rapidsync.lightningdevkit.org/snapshot".to_string());

        // Keep the pathfinding scorer warm by probing toward the network's
        // most-connected nodes, so the gateway can route before a user's
        // invoice arrives. Timing uses the upstream defaults (10s interval,
        // 1h per-node cooldown).
        node_builder.set_probing_config(
            ldk_node::probing::ProbingConfigBuilder::high_degree(opts.probe_top_node_count)
                .diversity_penalty_msat(opts.probe_diversity_penalty_msat)
                .max_locked_msat(opts.probe_max_locked_msat)
                .amount_range_msat(opts.probe_min_amount_msat, opts.probe_max_amount_msat)
                .build(),
        );
    }

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
    node_builder.set_runtime(runtime.handle().clone());

    info!(data_dir = %data_dir_str, alias = %opts.ldk_alias, "Starting LDK Node...");
    let node = Arc::new(node_builder.build()?);
    node.start()
        .map_err(|err| anyhow!("Failed to start LDK Node: {}", err.fmt_compact()))?;

    info!("Successfully started LDK Node");

    // On a fresh node with no persisted peers yet, seed connections to a few
    // large, well-run public nodes so the node participates in the network
    // right away.
    if opts.network == Network::Bitcoin && node.list_peers().is_empty() {
        for &(name, node_id, address) in connect::PUBLIC_NODES {
            let node_id = node_id.parse().expect("node id is valid");
            let address = SocketAddress::from_str(address).expect("address is valid");

            if let Err(err) = node.connect(node_id, address, true) {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), name, "Failed to connect to public node");
            }
        }
    }

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
