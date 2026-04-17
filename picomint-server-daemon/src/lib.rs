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
//! This crate hosts both the daemon library and the `picomint-server-daemon`
//! binary (`src/main.rs`). It drives config generation, consensus, and the
//! admin UI/CLI for the fixed module set (mint + lightning + wallet).

extern crate picomint_core;

pub mod cli;
pub mod config;
pub mod consensus;
pub mod p2p;
pub mod ui;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

/// Name of the server daemon's database file on disk.
pub const DB_FILE: &str = "database.redb";

use anyhow::Context;
use config::ServerConfig;
use picomint_bitcoin_rpc::BitcoinBackend;
use picomint_core::module::ApiAuth;
use picomint_core::task::TaskGroup;
use picomint_logging::LOG_CONSENSUS;
use picomint_redb::Database;
pub use picomint_server_core as core;
use tokio::net::TcpListener;
use tracing::info;

use crate::config::ConfigGenSettings;
use crate::config::db::{load_server_config, store_server_config};
use crate::config::setup::SetupApi;
use crate::p2p::{
    P2PConnector, P2PMessage, P2PStatusReceivers, ReconnectP2PConnections, p2p_status_channels,
};

#[allow(clippy::too_many_arguments)]
pub async fn run_server(
    auth: ApiAuth,
    settings: ConfigGenSettings,
    db: Database,
    code_version_str: String,
    task_group: TaskGroup,
    bitcoin_rpc: Arc<BitcoinBackend>,
    max_connections: usize,
    max_requests_per_connection: usize,
    cli_port: u16,
) -> anyhow::Result<()> {
    // Single channel for foreign (non-peer) iroh connections — fed by the
    // p2p accept loop's demux, drained by the consensus-phase api task.
    // Small bound: pre-DKG there's no consumer, so incoming api attempts
    // overflow and are dropped (no valid client should be talking to a
    // not-yet-bootstrapped federation).
    let (foreign_conn_tx, foreign_conn_rx) = async_channel::bounded(128);

    let (cfg, connections, p2p_status_receivers) = match load_server_config(&db).await {
        Some(cfg) => {
            let connector = P2PConnector::new(
                cfg.private.iroh_sk.clone(),
                settings.p2p_addr,
                cfg.consensus
                    .iroh_endpoints
                    .iter()
                    .map(|(peer, endpoints)| (*peer, endpoints.node_id))
                    .collect(),
            )
            .await?;

            let (p2p_status_senders, p2p_status_receivers) = p2p_status_channels(connector.peers());

            let connections = ReconnectP2PConnections::<P2PMessage>::new(
                cfg.private.identity,
                connector,
                &task_group,
                p2p_status_senders,
                foreign_conn_tx,
            );

            (cfg, connections, p2p_status_receivers)
        }
        None => {
            Box::pin(run_config_gen(
                db.clone(),
                settings.clone(),
                &task_group,
                auth.clone(),
                cli_port,
                foreign_conn_tx,
            ))
            .await?
        }
    };

    info!(target: LOG_CONSENSUS, "Starting consensus...");

    Box::pin(consensus::run(
        auth,
        connections,
        p2p_status_receivers,
        foreign_conn_rx,
        cfg,
        db,
        &task_group,
        code_version_str,
        bitcoin_rpc,
        settings.ui_addr,
        max_connections,
        max_requests_per_connection,
        cli_port,
    ))
    .await?;

    info!(target: LOG_CONSENSUS, "Shutting down tasks...");

    task_group.shutdown();

    Ok(())
}

pub async fn run_config_gen(
    db: Database,
    settings: ConfigGenSettings,
    task_group: &TaskGroup,
    auth: ApiAuth,
    cli_port: u16,
    foreign_conn_tx: async_channel::Sender<iroh::endpoint::Connection>,
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

    let ui_listener = TcpListener::bind(settings.ui_addr)
        .await
        .expect("Failed to bind setup UI");

    ui_task_group.spawn("setup-ui", move |handle| async move {
        axum::serve(ui_listener, ui_service)
            .with_graceful_shutdown(handle.make_shutdown_rx())
            .await
            .expect("Failed to serve setup UI");
    });

    info!(target: LOG_CONSENSUS, "Setup UI running at http://{} 🚀", settings.ui_addr);

    let cli_task_group = TaskGroup::new();
    let cli_state = cli::CliState {
        setup_api: setup_api.clone(),
    };
    let cli_bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), cli_port);
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
        cg_params.iroh_sk.clone(),
        settings.p2p_addr,
        cg_params
            .iroh_endpoints()
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.node_id))
            .collect(),
    )
    .await?;

    let (p2p_status_senders, p2p_status_receivers) = p2p_status_channels(connector.peers());

    let connections = ReconnectP2PConnections::<P2PMessage>::new(
        cg_params.identity,
        connector,
        task_group,
        p2p_status_senders,
        foreign_conn_tx,
    );

    let cfg = ServerConfig::distributed_gen(
        &cg_params,
        connections.clone(),
        p2p_status_receivers.clone(),
    )
    .await?;

    store_server_config(&db, &cfg).await;

    Ok((cfg, connections, p2p_status_receivers))
}
