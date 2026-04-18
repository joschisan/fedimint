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

use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Name of the server daemon's database file on disk.
pub const DB_FILE: &str = "database.redb";

use anyhow::Context;
use config::ServerConfig;
use picomint_bitcoin_rpc::BitcoinBackend;
use picomint_core::task::TaskGroup;
use picomint_logging::LOG_CONSENSUS;
use picomint_redb::Database;
use tokio::net::TcpListener;
use tracing::info;

/// Dispatch helper for module `handle_api` match arms.
///
/// `handler!(fn_name, self, req).await` decodes `req` into the parameter type
/// of `rpc::fn_name`, calls `rpc::fn_name(self, param).await`, and
/// consensus-encodes the response. Each module has a `mod rpc` submodule with
/// one `async fn name(module: &Self, param: P) -> Result<R, ApiError>` per
/// endpoint.
#[macro_export]
macro_rules! handler {
    ($func:ident, $self:expr, $req:expr) => {
        async move {
            let param = $req
                .to_typed()
                .map_err(|e| ::picomint_core::module::ApiError::bad_request(e.to_string()))?;
            let resp = rpc::$func($self, param).await?;
            ::std::result::Result::Ok(
                ::picomint_encoding::Encodable::consensus_encode_to_vec(&resp),
            )
        }
    };
}

use crate::config::db::{load_server_config, store_server_config};
use crate::config::setup::SetupApi;
use crate::config::{ConfigGenSettings, SetupResult};
use crate::p2p::{
    P2PConnector, P2PMessage, P2PStatusReceivers, ReconnectP2PConnections, p2p_status_channels,
};

pub async fn run_server(
    settings: ConfigGenSettings,
    db: Database,
    task_group: TaskGroup,
    bitcoin_rpc: Arc<BitcoinBackend>,
    data_dir: PathBuf,
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
                &data_dir,
                foreign_conn_tx,
            ))
            .await?
        }
    };

    info!(target: LOG_CONSENSUS, "Starting consensus...");

    Box::pin(consensus::run(
        connections,
        p2p_status_receivers,
        foreign_conn_rx,
        cfg,
        db,
        &task_group,
        bitcoin_rpc,
        settings.ui_config,
        &data_dir,
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
    data_dir: &Path,
    foreign_conn_tx: async_channel::Sender<iroh::endpoint::Connection>,
) -> anyhow::Result<(
    ServerConfig,
    ReconnectP2PConnections<P2PMessage>,
    P2PStatusReceivers,
)> {
    info!(target: LOG_CONSENSUS, "Starting config gen");

    let (setup_sender, mut setup_receiver) = tokio::sync::mpsc::channel(1);

    let setup_api = Arc::new(SetupApi::new(settings.clone(), setup_sender));

    let ui_task_group = TaskGroup::new();

    if let Some((ui_addr, auth)) = settings.ui_config.clone() {
        let ui_service = ui::setup::router(setup_api.clone(), auth).into_make_service();
        let ui_listener = TcpListener::bind(ui_addr)
            .await
            .expect("Failed to bind setup UI");
        ui_task_group.spawn("setup-ui", move |handle| async move {
            axum::serve(ui_listener, ui_service)
                .with_graceful_shutdown(handle.make_shutdown_rx())
                .await
                .expect("Failed to serve setup UI");
        });
        info!(target: LOG_CONSENSUS, "Setup UI running at http://{ui_addr} 🚀");
    } else {
        info!(target: LOG_CONSENSUS, "UI disabled (UI_ADDR unset); driving setup via CLI only");
    }

    let cli_task_group = TaskGroup::new();
    let cli_state = cli::CliState {
        setup_api: setup_api.clone(),
    };
    let data_dir = data_dir.to_owned();
    cli_task_group.spawn("setup-cli", move |handle| async move {
        cli::run_cli(&data_dir, cli_state, handle).await;
    });

    let setup_result = setup_receiver
        .recv()
        .await
        .expect("Setup result receiver closed unexpectedly");

    ui_task_group
        .shutdown_join_all(None)
        .await
        .context("Failed to shutdown UI server after config gen")?;

    cli_task_group
        .shutdown_join_all(None)
        .await
        .context("Failed to shutdown CLI server after config gen")?;

    match setup_result {
        SetupResult::Dkg(cg_params) => {
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

            let (p2p_status_senders, p2p_status_receivers) =
                p2p_status_channels(connector.peers());

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
        SetupResult::Restored(cfg) => {
            let cfg = *cfg;

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

            let (p2p_status_senders, p2p_status_receivers) =
                p2p_status_channels(connector.peers());

            let connections = ReconnectP2PConnections::<P2PMessage>::new(
                cfg.private.identity,
                connector,
                task_group,
                p2p_status_senders,
                foreign_conn_tx,
            );

            store_server_config(&db, &cfg).await;

            Ok((cfg, connections, p2p_status_receivers))
        }
    }
}
