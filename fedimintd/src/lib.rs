#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::large_futures)]

use std::convert::Infallible;
use std::env;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Context as _;
use bitcoin::Network;
use clap::{ArgGroup, Parser};
use fedimint_core::db::Database;
use fedimint_core::envs::{
    FM_IROH_DNS_ENV, FM_IROH_RELAY_ENV, FM_USE_UNKNOWN_MODULE_ENV, is_env_var_set,
};
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::task::TaskGroup;
use fedimint_core::timing;
use fedimint_core::util::{FmtCompactAnyhow as _, SafeUrl, handle_version_hash_command};
use fedimint_logging::{LOG_CORE, TracingSetup};
use fedimint_redb::RedbDatabase;
use fedimint_server::config::ConfigGenSettings;
use fedimint_server::config::io::DB_FILE;
use fedimint_server::core::ServerModuleInitRegistry;
use fedimint_server_bitcoin_rpc::BitcoindClientWithFallback;
use fedimint_server_bitcoin_rpc::bitcoind::BitcoindClient;
use fedimint_server_bitcoin_rpc::esplora::EsploraClient;
use fedimint_server_core::ServerModuleInitRegistryExt;
use fedimint_server_core::bitcoin_rpc::IServerBitcoinRpc;
use fedimint_unknown_server::UnknownInit;
use fedimintd_envs::{
    FM_BIND_P2P_ENV, FM_BIND_TOKIO_CONSOLE_ENV, FM_BIND_UI_ENV, FM_BITCOIN_NETWORK_ENV,
    FM_BITCOIND_PASSWORD_ENV, FM_BITCOIND_URL_ENV, FM_BITCOIND_URL_PASSWORD_FILE_ENV,
    FM_BITCOIND_USERNAME_ENV, FM_DATA_DIR_ENV, FM_ESPLORA_URL_ENV, FM_MAX_CONNECTIONS_ENV,
    FM_MAX_REQUESTS_PER_CONNECTION_ENV, FM_UI_PASSWORD_ENV,
};
use futures::FutureExt as _;
use tracing::{debug, error, info};

/// Time we will wait before forcefully shutting down tasks
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
    #[arg(long = "data-dir", env = FM_DATA_DIR_ENV)]
    data_dir: PathBuf,

    /// The bitcoin network of the federation
    #[arg(long, env = FM_BITCOIN_NETWORK_ENV, default_value = "regtest")]
    bitcoin_network: Network,

    /// Esplora HTTP base URL, e.g. <https://mempool.space/api>
    #[arg(long, env = FM_ESPLORA_URL_ENV)]
    esplora_url: Option<SafeUrl>,

    /// Bitcoind RPC URL, e.g. <http://127.0.0.1:8332>
    /// This should not include authentication parameters, they should be
    /// included in `FM_BITCOIND_USERNAME` and `FM_BITCOIND_PASSWORD`
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
    ///
    /// This is useful for setups that provide secret material via ramdisk
    /// e.g. SOPS, age, etc.
    ///
    /// Note this is not meant to handle bitcoind's cookie file.
    #[arg(long, env = FM_BITCOIND_URL_PASSWORD_FILE_ENV)]
    bitcoind_url_password_file: Option<PathBuf>,

    /// Address we bind to for p2p consensus communication
    ///
    /// Should be `0.0.0.0:8173` most of the time, as p2p connectivity is public
    /// and direct, and the port should be open it in the firewall.
    #[arg(long, env = FM_BIND_P2P_ENV, default_value = "0.0.0.0:8173")]
    bind_p2p: SocketAddr,

    /// Address we bind to for exposing the Web UI
    ///
    /// Built-in web UI is exposed as an HTTP port, and typically should
    /// have TLS terminated by Nginx/Traefik/etc. and forwarded to the locally
    /// bind port.
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
    pub async fn get_bitcoind_url_and_password(&self) -> anyhow::Result<(SafeUrl, String)> {
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

/// Block the thread and run a Fedimintd server
///
/// # Arguments
///
/// * `module_init_registry` - The registry of available modules.
///
/// * `code_version_hash` - The git hash of the code that the `fedimintd` binary
///   is being built from. This is used mostly for information purposes
///   (`fedimintd version-hash`). See `fedimint-build` crate for easy way to
///   obtain it.
fn dashboard_cli_router(api: fedimint_server_core::dashboard_ui::DynDashboardApi) -> axum::Router {
    use axum::Json;
    use axum::extract::State;
    use axum::routing::post;
    use fedimint_lnv2_server::Lightning;
    use fedimint_server::cli::CliError;
    use fedimint_server_cli_core::{
        AuditResponse, InviteResponse, Lnv2GatewayRequest, ROUTE_AUDIT, ROUTE_INVITE,
        ROUTE_MODULE_LNV2_GATEWAY_ADD, ROUTE_MODULE_LNV2_GATEWAY_LIST,
        ROUTE_MODULE_LNV2_GATEWAY_REMOVE, ROUTE_MODULE_WALLET_BLOCK_COUNT,
        ROUTE_MODULE_WALLET_FEERATE, ROUTE_MODULE_WALLET_PENDING_TX_CHAIN,
        ROUTE_MODULE_WALLET_TOTAL_VALUE, ROUTE_MODULE_WALLET_TX_CHAIN, WalletBlockCountResponse,
        WalletFeerateResponse, WalletTotalValueResponse,
    };
    use fedimint_server_core::dashboard_ui::{DashboardApiModuleExt, DynDashboardApi};
    use fedimint_walletv2_server::Wallet;

    async fn invite(State(api): State<DynDashboardApi>) -> Result<Json<InviteResponse>, CliError> {
        Ok(Json(InviteResponse {
            invite_code: api.federation_invite_code().await,
        }))
    }

    async fn audit(State(api): State<DynDashboardApi>) -> Result<Json<AuditResponse>, CliError> {
        Ok(Json(AuditResponse {
            audit: api.federation_audit().await,
        }))
    }

    async fn wallet_total_value(
        State(api): State<DynDashboardApi>,
    ) -> Result<Json<WalletTotalValueResponse>, CliError> {
        let wallet = api
            .get_module::<Wallet>()
            .ok_or_else(|| CliError::internal("Wallet module not found"))?;
        Ok(Json(WalletTotalValueResponse {
            total_value_sats: wallet
                .federation_wallet_ui()
                .await
                .map(|w| w.value.to_sat()),
        }))
    }

    async fn wallet_block_count(
        State(api): State<DynDashboardApi>,
    ) -> Result<Json<WalletBlockCountResponse>, CliError> {
        let wallet = api
            .get_module::<Wallet>()
            .ok_or_else(|| CliError::internal("Wallet module not found"))?;
        Ok(Json(WalletBlockCountResponse {
            block_count: wallet.consensus_block_count_ui().await,
        }))
    }

    async fn wallet_feerate(
        State(api): State<DynDashboardApi>,
    ) -> Result<Json<WalletFeerateResponse>, CliError> {
        let wallet = api
            .get_module::<Wallet>()
            .ok_or_else(|| CliError::internal("Wallet module not found"))?;
        Ok(Json(WalletFeerateResponse {
            sats_per_vbyte: wallet.consensus_feerate_ui().await,
        }))
    }

    async fn wallet_pending_tx_chain(
        State(api): State<DynDashboardApi>,
    ) -> Result<Json<Vec<fedimint_walletv2_common::TxInfo>>, CliError> {
        let wallet = api
            .get_module::<Wallet>()
            .ok_or_else(|| CliError::internal("Wallet module not found"))?;
        Ok(Json(wallet.pending_tx_chain_ui().await))
    }

    async fn wallet_tx_chain(
        State(api): State<DynDashboardApi>,
    ) -> Result<Json<Vec<fedimint_walletv2_common::TxInfo>>, CliError> {
        let wallet = api
            .get_module::<Wallet>()
            .ok_or_else(|| CliError::internal("Wallet module not found"))?;
        Ok(Json(wallet.tx_chain_ui().await))
    }

    async fn lnv2_gateway_add(
        State(api): State<DynDashboardApi>,
        Json(payload): Json<Lnv2GatewayRequest>,
    ) -> Result<Json<bool>, CliError> {
        let lnv2 = api
            .get_module::<Lightning>()
            .ok_or_else(|| CliError::internal("LNv2 module not found"))?;
        let url: fedimint_core::util::SafeUrl = payload
            .url
            .parse()
            .map_err(|e| CliError::internal(format!("Invalid URL: {e}")))?;
        Ok(Json(lnv2.add_gateway_ui(url).await))
    }

    async fn lnv2_gateway_remove(
        State(api): State<DynDashboardApi>,
        Json(payload): Json<Lnv2GatewayRequest>,
    ) -> Result<Json<bool>, CliError> {
        let lnv2 = api
            .get_module::<Lightning>()
            .ok_or_else(|| CliError::internal("LNv2 module not found"))?;
        let url: fedimint_core::util::SafeUrl = payload
            .url
            .parse()
            .map_err(|e| CliError::internal(format!("Invalid URL: {e}")))?;
        Ok(Json(lnv2.remove_gateway_ui(url).await))
    }

    async fn lnv2_gateway_list(
        State(api): State<DynDashboardApi>,
    ) -> Result<Json<Vec<fedimint_core::util::SafeUrl>>, CliError> {
        let lnv2 = api
            .get_module::<Lightning>()
            .ok_or_else(|| CliError::internal("LNv2 module not found"))?;
        Ok(Json(lnv2.gateways_ui().await))
    }

    axum::Router::new()
        .route(ROUTE_INVITE, post(invite))
        .route(ROUTE_AUDIT, post(audit))
        .route(ROUTE_MODULE_WALLET_TOTAL_VALUE, post(wallet_total_value))
        .route(ROUTE_MODULE_WALLET_BLOCK_COUNT, post(wallet_block_count))
        .route(ROUTE_MODULE_WALLET_FEERATE, post(wallet_feerate))
        .route(
            ROUTE_MODULE_WALLET_PENDING_TX_CHAIN,
            post(wallet_pending_tx_chain),
        )
        .route(ROUTE_MODULE_WALLET_TX_CHAIN, post(wallet_tx_chain))
        .route(ROUTE_MODULE_LNV2_GATEWAY_ADD, post(lnv2_gateway_add))
        .route(ROUTE_MODULE_LNV2_GATEWAY_REMOVE, post(lnv2_gateway_remove))
        .route(ROUTE_MODULE_LNV2_GATEWAY_LIST, post(lnv2_gateway_list))
        .with_state(api)
}

/// * `code_version_vendor_suffix` - An optional suffix that will be appended to
///   the internal fedimint release version, to distinguish binaries built by
///   different vendors, usually with a different set of modules. Currently DKG
///   will enforce that the combined `code_version` is the same between all
///   peers.
#[allow(clippy::too_many_lines)]
pub async fn run(
    module_init_registry: ServerModuleInitRegistry,
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
        available_modules: module_init_registry.kinds(),
        default_modules: module_init_registry.default_modules(),
    };

    let db = fedimint_core::db::v2::Database::open(server_opts.data_dir.join(DB_FILE))
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
    root_task_group.spawn_cancellable("main", async move {
        fedimint_server::run(
            server_opts.data_dir,
            ui_password,
            settings,
            db,
            code_version_str,
            module_init_registry,
            task_group,
            dyn_server_bitcoin_rpc,
            Box::new(fedimint_server_ui::setup::router),
            Box::new(fedimint_server_ui::dashboard::router),
            Box::new(dashboard_cli_router),
            server_opts.max_connections,
            server_opts.max_requests_per_connection,
            server_opts.bind_cli,
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

    std::process::exit(-1);
}

pub fn default_modules() -> ServerModuleInitRegistry {
    let mut server_gens = ServerModuleInitRegistry::new();

    server_gens.attach(fedimint_mintv2_server::MintInit);
    server_gens.attach(fedimint_walletv2_server::WalletInit);
    server_gens.attach(fedimint_lnv2_server::LightningInit);

    if is_env_var_set(FM_USE_UNKNOWN_MODULE_ENV) {
        server_gens.attach(UnknownInit);
    }

    server_gens
}
