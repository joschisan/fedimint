pub mod aleph_bft;
pub mod api;
pub mod db;
pub mod debug;
pub mod engine;
mod rpc;
pub mod server;
pub mod transaction;

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt;
use iroh::endpoint::{RecvStream, SendStream};
use picomint_api_client::transaction::ConsensusItem;
use picomint_api_client::wire;
use picomint_bitcoin_rpc::{BitcoinBackend, BitcoinRpcMonitor};
use picomint_core::NumPeers;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::envs::is_running_in_test_env;
use picomint_core::module::{ApiAuth, ApiError, ApiMethod, IrohApiRequest, ModuleCommon};
use picomint_core::task::{TaskGroup, sleep};
use picomint_core::util::FmtCompactAnyhow as _;
use picomint_logging::{LOG_CONSENSUS, LOG_CORE, LOG_NET_API};
use picomint_redb::Database;
use picomint_server_core::ServerModule;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tracing::{info, warn};

use crate::config::ServerConfig;
use crate::consensus::api::ConsensusApi;
use crate::consensus::engine::ConsensusEngine;
use crate::p2p::{P2PMessage, P2PStatusReceivers, ReconnectP2PConnections};

/// How many txs can be stored in memory before blocking the API
const TRANSACTION_BUFFER: usize = 1000;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    auth: ApiAuth,
    connections: ReconnectP2PConnections<P2PMessage>,
    p2p_status_receivers: P2PStatusReceivers,
    foreign_conn_rx: async_channel::Receiver<iroh::endpoint::Connection>,
    cfg: ServerConfig,
    db: Database,
    task_group: &TaskGroup,
    code_version_str: String,
    bitcoin_backend: Arc<BitcoinBackend>,
    ui_bind: SocketAddr,
    max_connections: usize,
    max_requests_per_connection: usize,
    cli_port: u16,
) -> anyhow::Result<()> {
    cfg.validate_config(&cfg.private.identity)?;

    let bitcoin_rpc_connection = BitcoinRpcMonitor::new(
        bitcoin_backend,
        if is_running_in_test_env() {
            Duration::from_millis(100)
        } else {
            Duration::from_mins(1)
        },
        task_group,
    );

    // Wait for the bitcoin backend to come up before instantiating modules that
    // read its status during startup (the wallet module broadcast loop).
    let _num_peers = NumPeers::from(cfg.consensus.iroh_endpoints.len());

    info!(target: LOG_CORE, "Initialise module mint...");
    let mint = Arc::new(picomint_mint_server::Mint::new(
        cfg.mint_config(),
        db.isolate("mint".to_string()),
    ));

    info!(target: LOG_CORE, "Initialise module ln...");
    let ln = Arc::new(picomint_ln_server::Lightning::new(
        cfg.ln_config(),
        db.isolate("ln".to_string()),
        bitcoin_rpc_connection.clone(),
    ));

    info!(target: LOG_CORE, "Initialise module wallet...");
    let wallet = Arc::new(picomint_wallet_server::Wallet::new(
        cfg.wallet_config(),
        db.isolate("wallet".to_string()),
        task_group,
        bitcoin_rpc_connection.clone(),
    ));

    let server = crate::consensus::server::Server { mint, ln, wallet };

    let client_cfg = cfg.consensus.clone();

    let (submission_sender, submission_receiver) = async_channel::bounded(TRANSACTION_BUFFER);
    let (shutdown_sender, shutdown_receiver) = watch::channel(None);
    let (ord_latency_sender, ord_latency_receiver) = watch::channel(None);

    let mut ci_status_senders = BTreeMap::new();
    let mut ci_status_receivers = BTreeMap::new();

    for peer in cfg.consensus.broadcast_public_keys.keys().copied() {
        let (ci_sender, ci_receiver) = watch::channel(None);

        ci_status_senders.insert(peer, ci_sender);
        ci_status_receivers.insert(peer, ci_receiver);
    }

    let consensus_api = Arc::new(ConsensusApi {
        cfg: cfg.clone(),
        db: db.clone(),
        server: server.clone(),
        client_cfg: client_cfg.clone(),
        submission_sender: submission_sender.clone(),
        shutdown_sender,
        shutdown_receiver: shutdown_receiver.clone(),
        p2p_status_receivers,
        ci_status_receivers,
        ord_latency_receiver,
        bitcoin_rpc_connection: bitcoin_rpc_connection.clone(),
        auth,
        code_version_str,
        task_group: task_group.clone(),
    });

    info!(target: LOG_CONSENSUS, "Starting Consensus Api...");

    task_group.spawn_cancellable(
        "iroh-api",
        run_iroh_api(
            consensus_api.clone(),
            foreign_conn_rx,
            task_group.clone(),
            max_connections,
            max_requests_per_connection,
        ),
    );

    info!(target: LOG_CONSENSUS, "Starting Submission of Module CI proposals...");

    submit_module_ci_proposals(
        task_group,
        "mint",
        consensus_api.server.mint.clone(),
        db.clone(),
        submission_sender.clone(),
        wire::ModuleConsensusItem::Mint,
    );
    submit_module_ci_proposals(
        task_group,
        "ln",
        consensus_api.server.ln.clone(),
        db.clone(),
        submission_sender.clone(),
        wire::ModuleConsensusItem::Ln,
    );
    submit_module_ci_proposals(
        task_group,
        "wallet",
        consensus_api.server.wallet.clone(),
        db.clone(),
        submission_sender.clone(),
        wire::ModuleConsensusItem::Wallet,
    );

    let ui_service = crate::ui::dashboard::router(consensus_api.clone()).into_make_service();

    let ui_listener = TcpListener::bind(ui_bind)
        .await
        .expect("Failed to bind dashboard UI");

    task_group.spawn("dashboard-ui", move |handle| async move {
        axum::serve(ui_listener, ui_service)
            .with_graceful_shutdown(handle.make_shutdown_rx())
            .await
            .expect("Failed to serve dashboard UI");
    });

    info!(target: LOG_CONSENSUS, "Dashboard UI running at http://{ui_bind} 🚀");

    {
        let cli_bind = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), cli_port);
        let dashboard_router = crate::cli::dashboard_cli_router(consensus_api.clone());
        task_group.spawn("consensus-cli", move |handle| async move {
            crate::cli::run_dashboard_cli(cli_bind, dashboard_router, handle).await;
        });
    }

    loop {
        match bitcoin_rpc_connection.status() {
            Some(status) => {
                if let Some(progress) = status.sync_progress {
                    if progress >= 0.999 {
                        break;
                    }

                    info!(target: LOG_CONSENSUS, "Waiting for bitcoin backend to sync... {progress:.1}%");
                } else {
                    break;
                }
            }
            None => {
                info!(target: LOG_CONSENSUS, "Waiting to connect to bitcoin backend...");
            }
        }

        sleep(Duration::from_secs(1)).await;
    }

    info!(target: LOG_CONSENSUS, "Starting Consensus Engine...");

    ConsensusEngine {
        db,
        cfg: cfg.clone(),
        connections,
        ord_latency_sender,
        ci_status_senders,
        submission_receiver,
        shutdown_receiver,
        server: consensus_api.server.clone(),
        task_group: task_group.clone(),
    }
    .run()
    .await?;

    Ok(())
}

const CONSENSUS_PROPOSAL_TIMEOUT: Duration = Duration::from_secs(30);

fn submit_module_ci_proposals<M>(
    task_group: &TaskGroup,
    namespace: &'static str,
    module: Arc<M>,
    db: Database,
    submission_sender: async_channel::Sender<ConsensusItem>,
    wrap: fn(<M::Common as ModuleCommon>::ConsensusItem) -> wire::ModuleConsensusItem,
) where
    M: ServerModule + Send + Sync + 'static,
{
    let mut interval = tokio::time::interval(Duration::from_secs(1));

    task_group.spawn(
        format!("citem_proposals_{namespace}"),
        move |task_handle| async move {
            while !task_handle.is_shutting_down() {
                let tx = db.begin_read().await;
                let view = tx.isolate(namespace.to_string());
                match tokio::time::timeout(
                    CONSENSUS_PROPOSAL_TIMEOUT,
                    module.consensus_proposal(&view),
                )
                .await
                {
                    Ok(items) => {
                        for item in items {
                            let _ = submission_sender
                                .send(ConsensusItem::Module(wrap(item)))
                                .await;
                        }
                    }
                    Err(_) => warn!(
                        target: LOG_CONSENSUS,
                        namespace,
                        "Module failed to propose consensus items on time",
                    ),
                }
                interval.tick().await;
            }
        },
    );
}

async fn run_iroh_api(
    consensus_api: Arc<ConsensusApi>,
    foreign_conn_rx: async_channel::Receiver<iroh::endpoint::Connection>,
    task_group: TaskGroup,
    max_connections: usize,
    max_requests_per_connection: usize,
) {
    let parallel_connections_limit = Arc::new(Semaphore::new(max_connections));

    while let Ok(connection) = foreign_conn_rx.recv().await {
        if parallel_connections_limit.available_permits() == 0 {
            warn!(
                target: LOG_NET_API,
                limit = max_connections,
                "Iroh API connection limit reached, blocking new connections"
            );
        }
        let permit = parallel_connections_limit
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore should not be closed");
        task_group.spawn_cancellable_silent(
            "handle-iroh-connection",
            handle_incoming(
                consensus_api.clone(),
                task_group.clone(),
                connection,
                permit,
                max_requests_per_connection,
            )
            .then(|result| async {
                if let Err(err) = result {
                    warn!(target: LOG_NET_API, err = %err.fmt_compact_anyhow(), "Failed to handle iroh connection");
                }
            }),
        );
    }
}

async fn handle_incoming(
    consensus_api: Arc<ConsensusApi>,
    task_group: TaskGroup,
    connection: iroh::endpoint::Connection,
    _connection_permit: tokio::sync::OwnedSemaphorePermit,
    max_requests_per_connection: usize,
) -> anyhow::Result<()> {
    let parallel_requests_limit = Arc::new(Semaphore::new(max_requests_per_connection));

    loop {
        let (send_stream, recv_stream) = connection.accept_bi().await?;

        if parallel_requests_limit.available_permits() == 0 {
            warn!(
                target: LOG_NET_API,
                limit = max_requests_per_connection,
                "Iroh API request limit reached for connection, blocking new requests"
            );
        }
        let permit = parallel_requests_limit
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore should not be closed");
        task_group.spawn_cancellable_silent(
            "handle-iroh-request",
            handle_request(consensus_api.clone(), send_stream, recv_stream, permit).then(
                |result| async {
                    if let Err(err) = result {
                        warn!(target: LOG_NET_API, err = %err.fmt_compact_anyhow(), "Failed to handle iroh request");
                    }
                },
            ),
        );
    }
}

async fn handle_request(
    consensus_api: Arc<ConsensusApi>,
    mut send_stream: SendStream,
    mut recv_stream: RecvStream,
    _request_permit: tokio::sync::OwnedSemaphorePermit,
) -> anyhow::Result<()> {
    let request = recv_stream.read_to_end(100_000).await?;
    let request = IrohApiRequest::consensus_decode_exact(&request)?;

    let response = dispatch(consensus_api, request).await;
    let response = response.consensus_encode_to_vec();

    send_stream.write_all(&response).await?;
    send_stream.finish()?;
    Ok(())
}

async fn dispatch(
    consensus_api: Arc<ConsensusApi>,
    request: IrohApiRequest,
) -> Result<Vec<u8>, ApiError> {
    match request.method {
        ApiMethod::Core(method) => consensus_api.handle_api(&method, request.request).await,
        ApiMethod::Mint(method) => {
            consensus_api
                .server
                .mint
                .handle_api(&method, request.request)
                .await
        }
        ApiMethod::Ln(method) => {
            consensus_api
                .server
                .ln
                .handle_api(&method, request.request)
                .await
        }
        ApiMethod::Wallet(method) => {
            consensus_api
                .server
                .wallet
                .handle_api(&method, request.request)
                .await
        }
    }
}
