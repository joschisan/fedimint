//! `picomint-server-daemon` process entry point.
//!
//! Parses CLI arguments, opens the database, wires up the bitcoin RPC, and
//! hands off to [`picomint_server_daemon::run_server`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use bitcoin::Network;
use clap::{ArgGroup, Parser};
use futures::FutureExt as _;
use picomint_core::rustls::install_crypto_provider;
use picomint_core::task::TaskGroup;
use picomint_core::timing;
use picomint_core::util::{FmtCompactAnyhow as _, SafeUrl};
use picomint_logging::{LOG_CORE, TracingSetup};
use picomint_server_bitcoin_rpc::BitcoindClientWithFallback;
use picomint_server_bitcoin_rpc::bitcoind::BitcoindClient;
use picomint_server_bitcoin_rpc::esplora::EsploraClient;
use picomint_server_core::bitcoin_rpc::IServerBitcoinRpc;
use picomint_server_daemon::config::ConfigGenSettings;
use picomint_server_daemon::{DB_FILE, run_server};
use tracing::{debug, error, info};

/// Time we will wait before forcefully shutting down tasks on exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

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
    #[arg(long = "data-dir", env = "DATA_DIR")]
    data_dir: PathBuf,

    /// The bitcoin network of the federation
    #[arg(long, env = "BITCOIN_NETWORK", default_value = "regtest")]
    bitcoin_network: Network,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = "ESPLORA_URL")]
    esplora_url: Option<SafeUrl>,

    /// Bitcoind RPC URL, e.g. <http://127.0.0.1:8332>
    #[arg(long, env = "BITCOIND_URL")]
    bitcoind_url: Option<SafeUrl>,

    /// The username to use when connecting to bitcoind
    #[arg(long, env = "BITCOIND_USERNAME")]
    bitcoind_username: Option<String>,

    /// The password to use when connecting to bitcoind
    #[arg(long, env = "BITCOIND_PASSWORD")]
    bitcoind_password: Option<String>,

    /// If set, the password part of `--bitcoind-url` will be set/replaced with
    /// the content of this file.
    #[arg(long, env = "BITCOIND_URL_PASSWORD_FILE")]
    bitcoind_url_password_file: Option<PathBuf>,

    /// Address we bind to for p2p consensus communication
    #[arg(long, env = "BIND_P2P", default_value = "0.0.0.0:8173")]
    bind_p2p: SocketAddr,

    /// Address we bind to for exposing the Web UI
    #[arg(long, env = "BIND_UI", default_value = "127.0.0.1:8174")]
    bind_ui: SocketAddr,

    /// Port for the CLI admin API (always binds 127.0.0.1, never public)
    #[arg(long, env = "CLI_PORT", default_value = "8175")]
    cli_port: u16,

    /// Password for the web UI (setup and dashboard)
    #[arg(long, env = "UI_PASSWORD")]
    ui_password: String,

    /// Optional URL of the Iroh DNS server
    #[arg(long, env = "IROH_DNS")]
    iroh_dns: Option<SafeUrl>,

    /// Optional URLs of the Iroh relays to use for registering
    #[arg(long, env = "IROH_RELAY", value_delimiter = ',')]
    iroh_relays: Vec<SafeUrl>,

    /// Enable tokio console logging
    #[arg(long, env = "BIND_TOKIO_CONSOLE")]
    bind_tokio_console: Option<SocketAddr>,

    /// Maximum number of concurrent Iroh API connections
    #[arg(long, env = "MAX_CONNECTIONS", default_value = "1000")]
    max_connections: usize,

    /// Maximum number of parallel requests per Iroh API connection
    #[arg(long, env = "MAX_REQUESTS_PER_CONNECTION", default_value = "50")]
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
                .expect("BITCOIND_URL is set but BITCOIND_PASSWORD is not");
            Ok((url, password))
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    let picomint_version = env!("CARGO_PKG_VERSION");

    let server_opts = ServerOpts::parse();

    let mut tracing_builder = TracingSetup::default();
    tracing_builder.tokio_console_bind(server_opts.bind_tokio_console);
    tracing_builder.init().unwrap();

    info!("Starting picomint-server-daemon (version: {picomint_version})");

    let code_version_str = picomint_version.to_string();

    let timing_total_runtime = timing::TimeReporter::new("total-runtime").info();

    let root_task_group = TaskGroup::new();

    let settings = ConfigGenSettings {
        p2p_bind: server_opts.bind_p2p,
        ui_bind: server_opts.bind_ui,
        iroh_dns: server_opts.iroh_dns.clone(),
        iroh_relays: server_opts.iroh_relays.clone(),
        network: server_opts.bitcoin_network,
    };

    let db = picomint_redb::Database::open(server_opts.data_dir.join(DB_FILE))
        .await
        .expect("Failed to open picomint-server-daemon database");

    let dyn_server_bitcoin_rpc = match (
        server_opts.bitcoind_url.as_ref(),
        server_opts.esplora_url.as_ref(),
    ) {
        (Some(_), None) => {
            let bitcoind_username = server_opts
                .bitcoind_username
                .clone()
                .expect("BITCOIND_URL is set but BITCOIND_USERNAME is not");
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
                .expect("BITCOIND_URL is set but BITCOIND_USERNAME is not");
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

    let ui_password = picomint_core::module::ApiAuth::new(server_opts.ui_password);

    let task_group = root_task_group.clone();
    let max_connections = server_opts.max_connections;
    let max_requests_per_connection = server_opts.max_requests_per_connection;
    let cli_port = server_opts.cli_port;

    root_task_group.spawn_cancellable("main", async move {
        run_server(
            ui_password,
            settings,
            db,
            code_version_str,
            task_group,
            dyn_server_bitcoin_rpc,
            max_connections,
            max_requests_per_connection,
            cli_port,
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

    picomint_logging::shutdown();

    drop(timing_total_runtime);

    std::process::exit(-1);
}
