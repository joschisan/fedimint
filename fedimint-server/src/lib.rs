#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_precision_loss)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::ref_option)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::needless_pass_by_value)]
#![allow(clippy::manual_let_else)]
#![allow(clippy::match_wildcard_for_single_variants)]
#![allow(clippy::trivially_copy_pass_by_ref)]

//! Server side fedimint module traits

extern crate fedimint_core;
pub mod cli;
pub mod db;

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::Context;
use config::ServerConfig;
use config::io::read_server_config;
use fedimint_core::config::P2PMessage;
use fedimint_core::db::{
    Database, IReadDatabaseTransactionOpsTyped, IWriteDatabaseTransactionOpsTyped as _,
    WriteDatabaseTransaction,
};
use fedimint_core::epoch::ConsensusItem;
use fedimint_core::module::ApiAuth;
use fedimint_core::net::peers::DynP2PConnections;
use fedimint_core::task::TaskGroup;
use fedimint_logging::LOG_CONSENSUS;
pub use fedimint_server_core as core;
use fedimint_server_core::ServerModuleInitRegistry;
use fedimint_server_core::bitcoin_rpc::DynServerBitcoinRpc;
use fedimint_server_core::dashboard_ui::DynDashboardApi;
use fedimint_server_core::setup_ui::{DynSetupApi, ISetupApi};
use iroh::Endpoint;
use iroh::endpoint::presets::N0;
use net::p2p::P2PStatusReceivers;
use net::p2p_connector::IrohConnector;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::ConfigGenSettings;
use crate::config::io::write_server_config;
use crate::config::setup::SetupApi;
use crate::db::{ServerInfo, ServerInfoKey};
use crate::fedimint_core::net::peers::IP2PConnections;
use crate::net::p2p::{ReconnectP2PConnections, p2p_status_channels};
use crate::net::p2p_connector::IP2PConnector;

/// The actual implementation of consensus
pub mod consensus;

/// Networking for mint-to-mint and client-to-mint communiccation
pub mod net;

/// Fedimint toplevel config
pub mod config;

/// A function/closure type for handling dashboard UI
pub type DashboardUiRouter = Box<dyn Fn(DynDashboardApi) -> axum::Router + Send>;
pub type DashboardCliRouter = Box<dyn Fn(DynDashboardApi) -> axum::Router + Send>;

/// A function/closure type for handling setup UI
pub type SetupUiRouter = Box<dyn Fn(DynSetupApi) -> axum::Router + Send>;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    data_dir: PathBuf,
    auth: ApiAuth,
    settings: ConfigGenSettings,
    db: Database,
    code_version_str: String,
    module_init_registry: ServerModuleInitRegistry,
    task_group: TaskGroup,
    bitcoin_rpc: DynServerBitcoinRpc,
    setup_ui_router: SetupUiRouter,
    dashboard_ui_router: DashboardUiRouter,
    module_cli_router: DashboardCliRouter,
    max_connections: usize,
    max_requests_per_connection: usize,
    cli_bind: std::net::SocketAddr,
) -> anyhow::Result<()> {
    let p2p_decoders: Arc<OnceLock<_>> = Arc::new(OnceLock::new());

    let (cfg, connections, p2p_status_receivers) = match get_config(&data_dir)? {
        Some(cfg) => {
            let connector = IrohConnector::new(
                cfg.private.iroh_p2p_sk.clone(),
                settings.p2p_bind,
                cfg.consensus
                    .iroh_endpoints
                    .iter()
                    .map(|(peer, endpoints)| (*peer, endpoints.p2p_pk))
                    .collect(),
                p2p_decoders.clone(),
            )
            .await?
            .into_dyn();

            let (p2p_status_senders, p2p_status_receivers) = p2p_status_channels(connector.peers());

            let connections = ReconnectP2PConnections::new(
                cfg.local.identity,
                connector,
                &task_group,
                p2p_status_senders,
            )
            .into_dyn();

            (cfg, connections, p2p_status_receivers)
        }
        None => {
            Box::pin(run_config_gen(
                data_dir.clone(),
                settings.clone(),
                db.clone(),
                &task_group,
                code_version_str.clone(),
                setup_ui_router,
                module_init_registry.clone(),
                auth.clone(),
                cli_bind,
                p2p_decoders.clone(),
            ))
            .await?
        }
    };

    let decoders = module_init_registry.decoders_strict(
        cfg.consensus
            .modules
            .iter()
            .map(|(id, config)| (*id, &config.kind)),
    )?;

    // Make module decoders available to the P2P layer so that frames carrying
    // DynModuleConsensusItem (e.g. SignedSessionOutcome) can be decoded.
    p2p_decoders
        .set(decoders.clone())
        .expect("p2p decoders were already set");

    let db = db.with_decoders(decoders);

    info!(target: LOG_CONSENSUS, "Starting consensus...");

    let client_endpoint = Endpoint::builder(N0).bind().await?;

    Box::pin(consensus::run(
        client_endpoint,
        auth,
        connections,
        p2p_status_receivers,
        cfg,
        db,
        module_init_registry.clone(),
        &task_group,
        code_version_str,
        bitcoin_rpc,
        settings.ui_bind,
        dashboard_ui_router,
        module_cli_router,
        max_connections,
        max_requests_per_connection,
        cli_bind,
    ))
    .await?;

    info!(target: LOG_CONSENSUS, "Shutting down tasks...");

    task_group.shutdown();

    Ok(())
}

async fn update_server_info_version_dbtx(
    dbtx: &mut WriteDatabaseTransaction<'_>,
    code_version_str: &str,
) {
    let mut server_info = dbtx.get_value(&ServerInfoKey).await.unwrap_or(ServerInfo {
        init_version: code_version_str.to_string(),
        last_version: code_version_str.to_string(),
    });
    server_info.last_version = code_version_str.to_string();
    dbtx.insert_entry(&ServerInfoKey, &server_info).await;
}

pub fn get_config(data_dir: &Path) -> anyhow::Result<Option<ServerConfig>> {
    if !data_dir.join("consensus.json").exists() {
        return Ok(None);
    }

    read_server_config(data_dir).map(Some)
}

#[allow(clippy::too_many_arguments)]
pub async fn run_config_gen(
    data_dir: PathBuf,
    settings: ConfigGenSettings,
    db: Database,
    task_group: &TaskGroup,
    code_version_str: String,
    setup_ui_handler: SetupUiRouter,
    module_init_registry: ServerModuleInitRegistry,
    auth: ApiAuth,
    cli_bind: std::net::SocketAddr,
    p2p_decoders: Arc<OnceLock<fedimint_core::module::registry::ModuleDecoderRegistry>>,
) -> anyhow::Result<(
    ServerConfig,
    DynP2PConnections<P2PMessage>,
    P2PStatusReceivers,
)> {
    info!(target: LOG_CONSENSUS, "Starting config gen");

    let (cgp_sender, mut cgp_receiver) = tokio::sync::mpsc::channel(1);

    let setup_api = SetupApi::new(settings.clone(), db.clone(), cgp_sender, auth);

    let ui_task_group = TaskGroup::new();

    let ui_service = setup_ui_handler(setup_api.clone().into_dyn()).into_make_service();

    let ui_listener = TcpListener::bind(settings.ui_bind)
        .await
        .expect("Failed to bind setup UI");

    ui_task_group.spawn("setup-ui", move |handle| async move {
        axum::serve(ui_listener, ui_service)
            .with_graceful_shutdown(handle.make_shutdown_rx())
            .await
            .expect("Failed to serve setup UI");
    });

    info!(target: LOG_CONSENSUS, "Setup UI running at http://{} 🚀", settings.ui_bind);

    let cli_task_group = TaskGroup::new();
    let cli_state = cli::CliState {
        setup_api: setup_api.clone().into_dyn(),
    };
    cli_task_group.spawn("setup-cli", move |handle| async move {
        cli::run_cli(cli_bind, cli_state, handle).await;
    });

    let cg_params = cgp_receiver
        .recv()
        .await
        .expect("Config gen params receiver closed unexpectedly");

    ui_task_group
        .shutdown_join_all(None)
        .await
        .context("Failed to shutdown UI server after config gen")?;

    cli_task_group
        .shutdown_join_all(None)
        .await
        .context("Failed to shutdown CLI server after config gen")?;

    let connector = IrohConnector::new(
        cg_params.iroh_p2p_sk.clone(),
        settings.p2p_bind,
        cg_params
            .iroh_endpoints()
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.p2p_pk))
            .collect(),
        p2p_decoders,
    )
    .await?
    .into_dyn();

    let (p2p_status_senders, p2p_status_receivers) = p2p_status_channels(connector.peers());

    let connections = ReconnectP2PConnections::new(
        cg_params.identity,
        connector,
        task_group,
        p2p_status_senders,
    )
    .into_dyn();

    let cfg = ServerConfig::distributed_gen(
        &cg_params,
        module_init_registry.clone(),
        code_version_str.clone(),
        connections.clone(),
        p2p_status_receivers.clone(),
    )
    .await?;

    write_server_config(&cfg, &data_dir)?;

    Ok((cfg, connections, p2p_status_receivers))
}
