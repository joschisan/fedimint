//! `picomint-server-daemon` process entry point.
//!
//! Parses CLI arguments, opens the database, wires up the bitcoin RPC, and
//! hands off to [`picomint_server_daemon::run_server`].

use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bitcoin::Network;
use clap::{ArgGroup, Parser};
use futures::FutureExt as _;
use picomint_bitcoin_rpc::{BitcoinBackend, BitcoindClient, EsploraClient};
use picomint_core::task::TaskGroup;
use picomint_core::util::{FmtCompactAnyhow as _, SafeUrl};
use picomint_logging::{LOG_CORE, TracingSetup};
use picomint_server_daemon::config::ConfigGenSettings;
use picomint_server_daemon::{DB_FILE, run_server};
use tracing::{debug, error, info};

/// Time we will wait before forcefully shutting down tasks on exit.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Parser)]
#[command(version)]
#[command(
    group(
        ArgGroup::new("bitcoind_auth")
            .args(["bitcoind_url"])
            .requires_all(["bitcoind_username", "bitcoind_password", "bitcoind_url"])
    ),
    group(
        ArgGroup::new("bitcoin_rpc")
            .required(true)
            .multiple(false)
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

    /// Address we bind to for iroh (p2p consensus + client API)
    #[arg(long = "p2p-addr", env = "P2P_ADDR", default_value = "0.0.0.0:8080")]
    p2p_addr: SocketAddr,

    /// Optional listen address for the Web UI. When unset the UI is
    /// disabled and all admin actions (including DKG setup) must go
    /// through the CLI.
    #[arg(long = "ui-addr", env = "UI_ADDR")]
    ui_addr: Option<SocketAddr>,

    /// Password for the web UI. Required when `UI_ADDR` is set,
    /// unused otherwise.
    #[arg(long, env = "UI_PASSWORD")]
    ui_password: Option<String>,

}

#[tokio::main]
async fn main() -> anyhow::Result<Infallible> {
    let picomint_version = env!("CARGO_PKG_VERSION");

    let server_opts = ServerOpts::parse();

    TracingSetup::default().init().unwrap();

    info!("Starting picomint-server-daemon (version: {picomint_version})");

    let root_task_group = TaskGroup::new();

    let ui_config = match (server_opts.ui_addr, server_opts.ui_password.clone()) {
        (Some(addr), Some(password)) => Some((addr, picomint_core::module::ApiAuth::new(password))),
        (None, _) => None,
        (Some(_), None) => {
            panic!("UI_ADDR is set but UI_PASSWORD is not; refusing to start the web UI without a password")
        }
    };

    let settings = ConfigGenSettings {
        p2p_addr: server_opts.p2p_addr,
        ui_config,
        network: server_opts.bitcoin_network,
    };

    let db = picomint_redb::Database::open(server_opts.data_dir.join(DB_FILE))
        .await
        .expect("Failed to open picomint-server-daemon database");

    let bitcoin_backend = Arc::new(
        match (
            server_opts.bitcoind_url.as_ref(),
            server_opts.esplora_url.as_ref(),
        ) {
            (Some(bitcoind_url), None) => {
                let bitcoind_username = server_opts
                    .bitcoind_username
                    .clone()
                    .expect("BITCOIND_URL is set but BITCOIND_USERNAME is not");
                let bitcoind_password = server_opts
                    .bitcoind_password
                    .clone()
                    .expect("BITCOIND_URL is set but BITCOIND_PASSWORD is not");
                BitcoinBackend::Bitcoind(
                    BitcoindClient::new(bitcoind_username, bitcoind_password, bitcoind_url)
                        .unwrap(),
                )
            }
            (None, Some(url)) => BitcoinBackend::Esplora(EsploraClient::new(url).unwrap()),
            _ => unreachable!("ArgGroup enforces exactly one of BITCOIND_URL or ESPLORA_URL"),
        },
    );

    root_task_group.install_kill_handler();

    tokio_rustls::rustls::crypto::ring::default_provider()
        .install_default()
        .ok();

    let task_group = root_task_group.clone();
    let data_dir = server_opts.data_dir.clone();

    root_task_group.spawn_cancellable("main", async move {
        run_server(settings, db, task_group, bitcoin_backend, data_dir)
            .await
            .unwrap_or_else(|err| {
                panic!("Main task returned error: {}", err.fmt_compact_anyhow())
            });
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

    std::process::exit(-1);
}
