//! `fedimintd` process entry point.
//!
//! Parses CLI arguments, opens the database, wires up the bitcoin RPC, and
//! hands off to [`crate::run_server`]. The actual daemon main is at
//! `src/bin/fedimintd.rs` and just calls [`run`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_core::envs::{FM_IROH_DNS_ENV, FM_IROH_RELAY_ENV};
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::task::TaskGroup;
use fedimint_core::timing;
use fedimint_core::util::{FmtCompactAnyhow as _, SafeUrl, handle_version_hash_command};
use fedimint_logging::{LOG_CORE, TracingSetup};
use fedimint_server_bitcoin_rpc::BitcoindClientWithFallback;
use fedimint_server_bitcoin_rpc::bitcoind::BitcoindClient;
use fedimint_server_bitcoin_rpc::esplora::EsploraClient;
use fedimint_server_core::bitcoin_rpc::IServerBitcoinRpc;
use fedimintd_envs::{
    FM_BIND_P2P_ENV, FM_BIND_TOKIO_CONSOLE_ENV, FM_BIND_UI_ENV, FM_BITCOIN_NETWORK_ENV,
    FM_BITCOIND_PASSWORD_ENV, FM_BITCOIND_URL_ENV, FM_BITCOIND_URL_PASSWORD_FILE_ENV,
    FM_BITCOIND_USERNAME_ENV, FM_DATA_DIR_ENV, FM_ESPLORA_URL_ENV, FM_MAX_CONNECTIONS_ENV,
    FM_MAX_REQUESTS_PER_CONNECTION_ENV, FM_UI_PASSWORD_ENV,
};
use futures::FutureExt as _;
use tracing::{debug, error, info};

use crate::config::ConfigGenSettings;
use crate::config::io::DB_FILE;

/// Time we will wait before forcefully shutting down tasks
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

pub fn default_network() -> Network {
    Network::Regtest
}

#[derive(Parser)]
#[command(version)]
#[command(
    group(
        ArgGroup::new("bitcoind_password_auth")
           .args(["bitcoind_password", "bitcoind_url_password_file"])
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
struct ServerOpts {
    /// Path to folder containing federation config files
    #[arg(long = "data-dir", env = FM_DATA_DIR_ENV)]
    data_dir: PathBuf,

    /// The bitcoin network of the federation
    #[arg(long, env = FM_BITCOIN_NETWORK_ENV, default_value = "regtest")]
    bitcoin_network: Network,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = FM_ESPLORA_URL_ENV)]
    esplora_url: Option<SafeUrl>,

    /// Bitcoind RPC URL, e.g. <http://127.0.0.1:8332>
    #[arg(long, env = FM_BITCOIND_URL_ENV)]
    bitcoind_url: Option<SafeUrl>,

    /// The username to use when connecting to bitcoind
    #[arg(long, env = FM_BITCOIND_USERNAME_ENV)]
    bitcoind_username: Option<String>,

    /// The password to use when connecting to bitcoind
    #[arg(long, env = FM_BITCOIND_PASSWORD_ENV)]
    bitcoind_password: Option<String>,

    /// If set, the password part of `--bitcoind-url` will be set/replaced with
    /// the content of this file.
    #[arg(long, env = FM_BITCOIND_URL_PASSWORD_FILE_ENV)]
    bitcoind_url_password_file: Option<PathBuf>,

    /// Address we bind to for p2p consensus communication
    #[arg(long, env = FM_BIND_P2P_ENV, default_value = "0.0.0.0:8173")]
    bind_p2p: SocketAddr,

    /// Address we bind to for exposing the Web UI
    #[arg(long, env = FM_BIND_UI_ENV, default_value = "127.0.0.1:8174")]
    bind_ui: SocketAddr,

    /// Address we bind to for the CLI admin API (localhost-only, no auth)
    #[arg(long, env = "FM_BIND_CLI", default_value = "127.0.0.1:8175")]
    bind_cli: SocketAddr,

    /// Password for the web UI (setup and dashboard)
    #[arg(long, env = FM_UI_PASSWORD_ENV)]
    ui_password: String,

    /// Optional URL of the Iroh DNS server
    #[arg(long, env = FM_IROH_DNS_ENV)]
    iroh_dns: Option<SafeUrl>,

    /// Optional URLs of the Iroh relays to use for registering
    #[arg(long, env = FM_IROH_RELAY_ENV, value_delimiter = ',')]
    iroh_relays: Vec<SafeUrl>,

    /// Enable tokio console logging
    #[arg(long, env = FM_BIND_TOKIO_CONSOLE_ENV)]
    bind_tokio_console: Option<SocketAddr>,

    /// Maximum number of concurrent Iroh API connections
    #[arg(long, env = FM_MAX_CONNECTIONS_ENV, default_value = "1000")]
    max_connections: usize,

    /// Maximum number of parallel requests per Iroh API connection
    #[arg(long, env = FM_MAX_REQUESTS_PER_CONNECTION_ENV, default_value = "50")]
    max_requests_per_connection: usize,
}

impl ServerOpts {
    async fn get_bitcoind_url_and_password(&self) -> anyhow::Result<(SafeUrl, String)> {
        let url = self
            .bitcoind_url
            .clone()
            .ok_or_else(|| anyhow::anyhow!("No bitcoind url set"))?;
        if let Some(password_file) = self.bitcoind_url_password_file.as_ref() {
            let password = tokio::fs::read_to_string(password_file)
                .await
                .context("Failed to read the password")?
                .trim()
                .to_owned();
            Ok((url, password))
        } else {
            let password = self
                .bitcoind_password
                .clone()
                .expect("FM_BITCOIND_URL is set but FM_BITCOIND_PASSWORD is not");
            Ok((url, password))
        }
    }
}

/// Block the thread and run a Fedimintd server.
///
/// `code_version_hash` is the git hash of the binary being built (see the
/// `fedimint-build` crate). It is surfaced via `fedimintd version-hash`.
/// `code_version_vendor_suffix` is an optional suffix appended to the internal
/// fedimint release version to distinguish binaries built by different vendors
/// with different module sets. DKG enforces that the combined `code_version` is
/// the same across all peers.
pub async fn run(
    code_version_hash: &str,
    code_version_vendor_suffix: Option<&str>,
) -> anyhow::Result<Infallible> {
    assert_eq!(
        env!("FEDIMINT_BUILD_CODE_VERSION").len(),
        code_version_hash.len(),
        "version_hash must have an expected length"
    );

    handle_version_hash_command(code_version_hash);

    let fedimint_version = env!("CARGO_PKG_VERSION");

    let server_opts = ServerOpts::parse();

    let mut tracing_builder = TracingSetup::default();
    tracing_builder.tokio_console_bind(server_opts.bind_tokio_console);
    tracing_builder.init().unwrap();

    info!("Starting fedimintd (version: {fedimint_version} version_hash: {code_version_hash})");

    let code_version_str = code_version_vendor_suffix.map_or_else(
        || fedimint_version.to_string(),
        |suffix| format!("{fedimint_version}+{suffix}"),
    );

    let timing_total_runtime = timing::TimeReporter::new("total-runtime").info();

    let root_task_group = TaskGroup::new();

    let settings = ConfigGenSettings {
        p2p_bind: server_opts.bind_p2p,
        ui_bind: server_opts.bind_ui,
        iroh_dns: server_opts.iroh_dns.clone(),
        iroh_relays: server_opts.iroh_relays.clone(),
        network: server_opts.bitcoin_network,
    };

    let db = fedimint_redb::Database::open(server_opts.data_dir.join(DB_FILE))
        .await
        .expect("Failed to open fedimintd database");

    let dyn_server_bitcoin_rpc = match (
        server_opts.bitcoind_url.as_ref(),
        server_opts.esplora_url.as_ref(),
    ) {
        (Some(_), None) => {
            let bitcoind_username = server_opts
                .bitcoind_username
                .clone()
                .expect("FM_BITCOIND_URL is set but FM_BITCOIND_USERNAME is not");
            let (bitcoind_url, bitcoind_password) = server_opts
                .get_bitcoind_url_and_password()
                .await
                .expect("Failed to get bitcoind url");
            BitcoindClient::new(bitcoind_username, bitcoind_password, &bitcoind_url)
                .unwrap()
                .into_dyn()
        }
        (None, Some(url)) => EsploraClient::new(url).unwrap().into_dyn(),
        (Some(_), Some(esplora_url)) => {
            let bitcoind_username = server_opts
                .bitcoind_username
                .clone()
                .expect("FM_BITCOIND_URL is set but FM_BITCOIND_USERNAME is not");
            let (bitcoind_url, bitcoind_password) = server_opts
                .get_bitcoind_url_and_password()
                .await
                .expect("Failed to get bitcoind url");
            BitcoindClientWithFallback::new(
                bitcoind_username,
                bitcoind_password,
                &bitcoind_url,
                esplora_url,
            )
            .unwrap()
            .into_dyn()
        }
        _ => unreachable!("ArgGroup already enforced XOR relation"),
    };

    root_task_group.install_kill_handler();

    install_crypto_provider().await;

    let ui_password = fedimint_core::module::ApiAuth::new(server_opts.ui_password);

    let task_group = root_task_group.clone();
    let data_dir = server_opts.data_dir.clone();
    let max_connections = server_opts.max_connections;
    let max_requests_per_connection = server_opts.max_requests_per_connection;
    let cli_bind = server_opts.bind_cli;

    root_task_group.spawn_cancellable("main", async move {
        crate::run_server(
            data_dir,
            ui_password,
            settings,
            db,
            code_version_str,
            task_group,
            dyn_server_bitcoin_rpc,
            Box::new(|api| fedimint_server_ui::setup::router(api)),
            Box::new(|api| fedimint_server_ui::dashboard::router(api)),
            Box::new(|api| crate::cli::dashboard_cli_router(api)),
            max_connections,
            max_requests_per_connection,
            cli_bind,
        )
        .await
        .unwrap_or_else(|err| panic!("Main task returned error: {}", err.fmt_compact_anyhow()));
    });

    let shutdown_future = root_task_group
        .make_handle()
        .make_shutdown_rx()
        .then(|()| async {
            info!(target: LOG_CORE, "Shutdown called");
        });

    shutdown_future.await;

    debug!(target: LOG_CORE, "Terminating main task");

    if let Err(err) = root_task_group.join_all(Some(SHUTDOWN_TIMEOUT)).await {
        error!(target: LOG_CORE, err = %err.fmt_compact_anyhow(), "Error while shutting down task group");
    }

    debug!(target: LOG_CORE, "Shutdown complete");

    fedimint_logging::shutdown();

    drop(timing_total_runtime);

    // Silence the `Arc` warning — `Arc` keeps `dyn_server_bitcoin_rpc` alive.
    let _ = Arc::<u32>::new(0);

    std::process::exit(-1);
}
