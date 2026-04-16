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

//! Federation server daemon.
//!
//! This crate hosts both the daemon library and the `fedimint-server-daemon`
//! binary (`src/main.rs`). It drives config generation, consensus, and the
//! admin UI/CLI for the fixed module set (mint + lightning + wallet).

extern crate fedimint_core;

pub mod cli;
pub mod config;
pub mod consensus;
pub mod p2p;
pub mod ui;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use config::ServerConfig;
use config::io::read_server_config;
use fedimint_core::module::ApiAuth;
use fedimint_core::task::TaskGroup;
use fedimint_logging::LOG_CONSENSUS;
use fedimint_redb::Database;
pub use fedimint_server_core as core;
use fedimint_server_core::bitcoin_rpc::DynServerBitcoinRpc;
use iroh::Endpoint;
use iroh::endpoint::presets::N0;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::ConfigGenSettings;
use crate::config::io::write_server_config;
use crate::config::setup::SetupApi;
use crate::p2p::{P2PConnector, P2PMessage, P2PStatusReceivers, ReconnectP2PConnections, p2p_status_channels};

#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    data_dir: PathBuf,
    auth: ApiAuth,
    settings: ConfigGenSettings,
    db: Database,
    code_version_str: String,
    task_group: TaskGroup,
    bitcoin_rpc: DynServerBitcoinRpc,
    max_connections: usize,
    max_requests_per_connection: usize,
    cli_bind: SocketAddr,
) -> anyhow::Result<()> {
    let (cfg, connections, p2p_status_receivers) = match get_config(&data_dir)? {
        Some(cfg) => {
            let connector = P2PConnector::new(
                cfg.private.iroh_p2p_sk.clone(),
                settings.p2p_bind,
                cfg.consensus
                    .iroh_endpoints
                    .iter()
                    .map(|(peer, endpoints)| (*peer, endpoints.p2p_pk))
                    .collect(),
            )
            .await?;

            let (p2p_status_senders, p2p_status_receivers) = p2p_status_channels(connector.peers());

            let connections = ReconnectP2PConnections::<P2PMessage>::new(
                cfg.local.identity,
                connector,
                &task_group,
                p2p_status_senders,
            );

            (cfg, connections, p2p_status_receivers)
        }
        None => {
            Box::pin(run_config_gen(
                data_dir.clone(),
                settings.clone(),
                &task_group,
                code_version_str.clone(),
                auth.clone(),
                cli_bind,
            ))
            .await?
        }
    };

    info!(target: LOG_CONSENSUS, "Starting consensus...");

    let client_endpoint = Endpoint::builder(N0).bind().await?;

    Box::pin(consensus::run(
        client_endpoint,
        auth,
        connections,
        p2p_status_receivers,
        cfg,
        db,
        &task_group,
        code_version_str,
        bitcoin_rpc,
        settings.ui_bind,
        max_connections,
        max_requests_per_connection,
        cli_bind,
    ))
    .await?;

    info!(target: LOG_CONSENSUS, "Shutting down tasks...");

    task_group.shutdown();

    Ok(())
}

pub fn get_config(data_dir: &Path) -> anyhow::Result<Option<ServerConfig>> {
    if !data_dir.join("consensus.json").exists() {
        return Ok(None);
    }

    read_server_config(data_dir).map(Some)
}

pub async fn run_config_gen(
    data_dir: PathBuf,
    settings: ConfigGenSettings,
    task_group: &TaskGroup,
    code_version_str: String,
    auth: ApiAuth,
    cli_bind: SocketAddr,
) -> anyhow::Result<(
    ServerConfig,
    ReconnectP2PConnections<P2PMessage>,
    P2PStatusReceivers,
)> {
    info!(target: LOG_CONSENSUS, "Starting config gen");

    let (cgp_sender, mut cgp_receiver) = tokio::sync::mpsc::channel(1);

    let setup_api = Arc::new(SetupApi::new(settings.clone(), cgp_sender, auth));

    let ui_task_group = TaskGroup::new();

    let ui_service = ui::setup::router(setup_api.clone()).into_make_service();

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
        setup_api: setup_api.clone(),
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

    let connector = P2PConnector::new(
        cg_params.iroh_p2p_sk.clone(),
        settings.p2p_bind,
        cg_params
            .iroh_endpoints()
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.p2p_pk))
            .collect(),
    )
    .await?;

    let (p2p_status_senders, p2p_status_receivers) = p2p_status_channels(connector.peers());

    let connections = ReconnectP2PConnections::<P2PMessage>::new(
        cg_params.identity,
        connector,
        task_group,
        p2p_status_senders,
    );

    let cfg = ServerConfig::distributed_gen(
        &cg_params,
        code_version_str.clone(),
        connections.clone(),
        p2p_status_receivers.clone(),
    )
    .await?;

    write_server_config(&cfg, &data_dir)?;

    Ok((cfg, connections, p2p_status_receivers))
}
