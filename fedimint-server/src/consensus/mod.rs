pub mod aleph_bft;
pub mod api;
pub mod db;
pub mod debug;
pub mod engine;
pub mod transaction;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use async_channel::Sender;
use fedimint_api_client::transaction::ConsensusItem;
use fedimint_api_client::wire;
use fedimint_core::NumPeers;
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::envs::is_running_in_test_env;
use fedimint_core::module::{
    ApiAuth, ApiEndpoint, ApiError, ApiMethod, FEDIMINT_API_ALPN, IrohApiRequest,
};
use fedimint_core::net::iroh::build_iroh_endpoint;
use fedimint_core::net::peers::DynP2PConnections;
use fedimint_core::task::{TaskGroup, sleep};
use fedimint_core::util::FmtCompactAnyhow as _;
use fedimint_logging::{LOG_CONSENSUS, LOG_CORE, LOG_NET_API};
use fedimint_redb::Database;
use fedimint_server_core::bitcoin_rpc::{DynServerBitcoinRpc, ServerBitcoinRpcMonitor};
use fedimint_server_core::dashboard_ui::IDashboardApi;
use fedimint_server_core::{DynGlobalApi, ServerModuleInitRegistry};
use futures::FutureExt;
use iroh::Endpoint;
use iroh::endpoint::{Incoming, RecvStream, SendStream};
use serde_json::Value;
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, watch};
use tracing::{info, warn};

use crate::DashboardUiRouter;
use crate::config::ServerConfig;
use crate::consensus::api::{ConsensusApi, server_endpoints};
use crate::consensus::engine::ConsensusEngine;
use crate::net::HasApiContext;
use crate::net::p2p::P2PStatusReceivers;
use crate::p2p::P2PMessage;

/// How many txs can be stored in memory before blocking the API
const TRANSACTION_BUFFER: usize = 1000;

#[allow(clippy::too_many_arguments)]
pub async fn run(
    connectors: Endpoint,
    auth: ApiAuth,
    connections: DynP2PConnections<P2PMessage>,
    p2p_status_receivers: P2PStatusReceivers,
    cfg: ServerConfig,
    db: Database,
    module_init_registry: ServerModuleInitRegistry,
    task_group: &TaskGroup,
    code_version_str: String,
    dyn_server_bitcoin_rpc: DynServerBitcoinRpc,
    ui_bind: SocketAddr,
    dashboard_ui_router: DashboardUiRouter,
    dashboard_cli_router: crate::DashboardCliRouter,
    max_connections: usize,
    max_requests_per_connection: usize,
    cli_bind: SocketAddr,
) -> anyhow::Result<()> {
    cfg.validate_config(&cfg.local.identity, &module_init_registry)?;

    let global_api = DynGlobalApi::new(
        connectors.clone(),
        cfg.consensus
            .api_endpoints()
            .iter()
            .map(|(&peer_id, url)| (peer_id, url.url.clone()))
            .collect(),
    );

    let bitcoin_rpc_connection = ServerBitcoinRpcMonitor::new(
        dyn_server_bitcoin_rpc,
        if is_running_in_test_env() {
            Duration::from_millis(100)
        } else {
            Duration::from_mins(1)
        },
        task_group,
    );

    // Directly instantiate the three canonical server modules from their typed
    // Init structs — skips the dynamic registry lookup entirely.
    let server = {
        use fedimint_api_client::wire::{LN_INSTANCE_ID, MINT_INSTANCE_ID, WALLET_INSTANCE_ID};
        use fedimint_lnv2_server::LightningInit;
        use fedimint_mintv2_server::MintInit;
        use fedimint_server_core::{ServerModuleInit, ServerModuleInitArgs};
        use fedimint_walletv2_server::WalletInit;

        let num_peers = NumPeers::from(cfg.consensus.api_endpoints().len());

        async fn init_one<I>(
            init: I,
            instance_id: ModuleInstanceId,
            cfg: &ServerConfig,
            db: &Database,
            task_group: &TaskGroup,
            num_peers: NumPeers,
            global_api: &DynGlobalApi,
            bitcoin_rpc: &ServerBitcoinRpcMonitor,
        ) -> anyhow::Result<I::Module>
        where
            I: ServerModuleInit,
        {
            let args = ServerModuleInitArgs::new(
                cfg.get_module_config(instance_id)?,
                db.isolate(format!("module-{instance_id}")),
                task_group.clone(),
                cfg.local.identity,
                num_peers,
                global_api.with_module(instance_id),
                bitcoin_rpc.clone(),
            );
            init.init(&args).await
        }

        info!(target: LOG_CORE, "Initialise module {MINT_INSTANCE_ID}...");
        let mint = Arc::new(
            init_one(
                MintInit,
                MINT_INSTANCE_ID,
                &cfg,
                &db,
                task_group,
                num_peers,
                &global_api,
                &bitcoin_rpc_connection,
            )
            .await?,
        );
        info!(target: LOG_CORE, "Initialise module {LN_INSTANCE_ID}...");
        let ln = Arc::new(
            init_one(
                LightningInit,
                LN_INSTANCE_ID,
                &cfg,
                &db,
                task_group,
                num_peers,
                &global_api,
                &bitcoin_rpc_connection,
            )
            .await?,
        );
        info!(target: LOG_CORE, "Initialise module {WALLET_INSTANCE_ID}...");
        let wallet = Arc::new(
            init_one(
                WalletInit,
                WALLET_INSTANCE_ID,
                &cfg,
                &db,
                task_group,
                num_peers,
                &global_api,
                &bitcoin_rpc_connection,
            )
            .await?,
        );

        crate::server::Server { mint, ln, wallet }
    };

    let client_cfg = cfg.consensus.to_client_config(&module_init_registry)?;

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

    let consensus_api = ConsensusApi {
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
    };

    info!(target: LOG_CONSENSUS, "Starting Consensus Api...");

    Box::pin(start_iroh_api(
        cfg.private.iroh_api_sk.clone(),
        consensus_api.clone(),
        task_group,
        max_connections,
        max_requests_per_connection,
    ))
    .await?;

    info!(target: LOG_CONSENSUS, "Starting Submission of Module CI proposals...");

    {
        use fedimint_api_client::wire::{LN_INSTANCE_ID, MINT_INSTANCE_ID, WALLET_INSTANCE_ID};

        submit_module_ci_proposals(
            task_group,
            db.clone(),
            MINT_INSTANCE_ID,
            ModuleKind::from_static_str("mintv2"),
            server.mint.clone(),
            wire::ModuleConsensusItem::Mint,
            submission_sender.clone(),
        );
        submit_module_ci_proposals(
            task_group,
            db.clone(),
            LN_INSTANCE_ID,
            ModuleKind::from_static_str("lnv2"),
            server.ln.clone(),
            wire::ModuleConsensusItem::Ln,
            submission_sender.clone(),
        );
        submit_module_ci_proposals(
            task_group,
            db.clone(),
            WALLET_INSTANCE_ID,
            ModuleKind::from_static_str("walletv2"),
            server.wallet.clone(),
            wire::ModuleConsensusItem::Wallet,
            submission_sender.clone(),
        );
    }

    let ui_service = dashboard_ui_router(consensus_api.clone().into_dyn()).into_make_service();

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
        let dashboard_router = dashboard_cli_router(consensus_api.clone().into_dyn());
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
        server,
        task_group: task_group.clone(),
    }
    .run()
    .await?;

    Ok(())
}

const CONSENSUS_PROPOSAL_TIMEOUT: Duration = Duration::from_secs(30);

fn submit_module_ci_proposals<M>(
    task_group: &TaskGroup,
    db: Database,
    module_id: ModuleInstanceId,
    kind: ModuleKind,
    module: Arc<M>,
    to_wire: fn(
        <M::Common as fedimint_core::module::ModuleCommon>::ConsensusItem,
    ) -> wire::ModuleConsensusItem,
    submission_sender: Sender<ConsensusItem>,
) where
    M: fedimint_server_core::ServerModule + Send + Sync + 'static,
{
    let mut interval = tokio::time::interval(if is_running_in_test_env() {
        Duration::from_millis(100)
    } else {
        Duration::from_secs(1)
    });

    task_group.spawn(
        format!("citem_proposals_{module_id}"),
        move |task_handle| async move {
            while !task_handle.is_shutting_down() {
                let tx = db.begin_read().await;
                let view = tx.isolate(format!("module-{module_id}"));
                let module_consensus_items = tokio::time::timeout(
                    CONSENSUS_PROPOSAL_TIMEOUT,
                    module.consensus_proposal(&view),
                )
                .await;
                drop(view);
                drop(tx);

                match module_consensus_items {
                    Ok(items) => {
                        for item in items {
                            if submission_sender
                                .send(ConsensusItem::Module(to_wire(item)))
                                .await
                                .is_err()
                            {
                                warn!(
                                    target: LOG_CONSENSUS,
                                    module_id,
                                    "Unable to submit module consensus item proposal via channel"
                                );
                            }
                        }
                    }
                    Err(..) => {
                        warn!(
                            target: LOG_CONSENSUS,
                            module_id,
                            %kind,
                            "Module failed to propose consensus items on time"
                        );
                    }
                }

                interval.tick().await;
            }
        },
    );
}

async fn start_iroh_api(
    secret_key: iroh::SecretKey,
    consensus_api: ConsensusApi,
    task_group: &TaskGroup,
    max_connections: usize,
    max_requests_per_connection: usize,
) -> anyhow::Result<()> {
    let endpoint = build_iroh_endpoint(
        secret_key,
        SocketAddr::from(([0, 0, 0, 0], 0)),
        FEDIMINT_API_ALPN,
    )
    .await?;
    task_group.spawn_cancellable(
        "iroh-api",
        run_iroh_api(
            consensus_api,
            endpoint,
            task_group.clone(),
            max_connections,
            max_requests_per_connection,
        ),
    );

    Ok(())
}

async fn run_iroh_api(
    consensus_api: ConsensusApi,
    endpoint: Endpoint,
    task_group: TaskGroup,
    max_connections: usize,
    max_requests_per_connection: usize,
) {
    use fedimint_lnv2_server::Lightning;
    use fedimint_mintv2_server::Mint;
    use fedimint_server_core::ServerModule;
    use fedimint_walletv2_server::Wallet;

    let core_api = server_endpoints()
        .into_iter()
        .map(|endpoint| (endpoint.path.to_string(), endpoint))
        .collect::<BTreeMap<String, ApiEndpoint<ConsensusApi>>>();

    fn build<M: ServerModule>(module: &M) -> BTreeMap<String, ApiEndpoint<M>> {
        module
            .api_endpoints()
            .into_iter()
            .map(|endpoint| (endpoint.path.to_string(), endpoint))
            .collect()
    }

    let mint_api = build::<Mint>(&consensus_api.server.mint);
    let ln_api = build::<Lightning>(&consensus_api.server.ln);
    let wallet_api = build::<Wallet>(&consensus_api.server.wallet);

    let consensus_api = Arc::new(consensus_api);
    let core_api = Arc::new(core_api);
    let mint_api = Arc::new(mint_api);
    let ln_api = Arc::new(ln_api);
    let wallet_api = Arc::new(wallet_api);
    let parallel_connections_limit = Arc::new(Semaphore::new(max_connections));

    loop {
        match endpoint.accept().await {
            Some(incoming) => {
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
                        core_api.clone(),
                        mint_api.clone(),
                        ln_api.clone(),
                        wallet_api.clone(),
                        task_group.clone(),
                        incoming,
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
            None => return,
        }
    }
}

type MintApi = BTreeMap<String, ApiEndpoint<fedimint_mintv2_server::Mint>>;
type LnApi = BTreeMap<String, ApiEndpoint<fedimint_lnv2_server::Lightning>>;
type WalletApi = BTreeMap<String, ApiEndpoint<fedimint_walletv2_server::Wallet>>;

#[allow(clippy::too_many_arguments)]
async fn handle_incoming(
    consensus_api: Arc<ConsensusApi>,
    core_api: Arc<BTreeMap<String, ApiEndpoint<ConsensusApi>>>,
    mint_api: Arc<MintApi>,
    ln_api: Arc<LnApi>,
    wallet_api: Arc<WalletApi>,
    task_group: TaskGroup,
    incoming: Incoming,
    _connection_permit: tokio::sync::OwnedSemaphorePermit,
    max_requests_per_connection: usize,
) -> anyhow::Result<()> {
    let connection = incoming.accept()?.await?;
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
            handle_request(
                consensus_api.clone(),
                core_api.clone(),
                mint_api.clone(),
                ln_api.clone(),
                wallet_api.clone(),
                send_stream,
                recv_stream,
                permit,
            )
            .then(|result| async {
                if let Err(err) = result {
                    warn!(target: LOG_NET_API, err = %err.fmt_compact_anyhow(), "Failed to handle iroh request");
                }
            }),
        );
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_request(
    consensus_api: Arc<ConsensusApi>,
    core_api: Arc<BTreeMap<String, ApiEndpoint<ConsensusApi>>>,
    mint_api: Arc<MintApi>,
    ln_api: Arc<LnApi>,
    wallet_api: Arc<WalletApi>,
    mut send_stream: SendStream,
    mut recv_stream: RecvStream,
    _request_permit: tokio::sync::OwnedSemaphorePermit,
) -> anyhow::Result<()> {
    let request = recv_stream.read_to_end(100_000).await?;

    let request = serde_json::from_slice::<IrohApiRequest>(&request)?;

    let response = await_response(
        consensus_api,
        core_api,
        mint_api,
        ln_api,
        wallet_api,
        request,
    )
    .await;

    let response = serde_json::to_vec(&response)?;

    send_stream.write_all(&response).await?;

    send_stream.finish()?;

    Ok(())
}

async fn await_response(
    consensus_api: Arc<ConsensusApi>,
    core_api: Arc<BTreeMap<String, ApiEndpoint<ConsensusApi>>>,
    mint_api: Arc<MintApi>,
    ln_api: Arc<LnApi>,
    wallet_api: Arc<WalletApi>,
    request: IrohApiRequest,
) -> Result<Value, ApiError> {
    use fedimint_api_client::wire::{LN_INSTANCE_ID, MINT_INSTANCE_ID, WALLET_INSTANCE_ID};

    match request.method {
        ApiMethod::Core(method) => {
            let endpoint = core_api.get(&method).ok_or(ApiError::not_found(method))?;
            let state = consensus_api.context(&request.request, None).await;
            (endpoint.handler)(state, request.request).await
        }
        ApiMethod::Module(module_id, method) => match module_id {
            MINT_INSTANCE_ID => {
                let endpoint = mint_api.get(&method).ok_or(ApiError::not_found(method))?;
                let state: &fedimint_mintv2_server::Mint = consensus_api
                    .context(&request.request, Some(module_id))
                    .await;
                (endpoint.handler)(state, request.request).await
            }
            LN_INSTANCE_ID => {
                let endpoint = ln_api.get(&method).ok_or(ApiError::not_found(method))?;
                let state: &fedimint_lnv2_server::Lightning = consensus_api
                    .context(&request.request, Some(module_id))
                    .await;
                (endpoint.handler)(state, request.request).await
            }
            WALLET_INSTANCE_ID => {
                let endpoint = wallet_api.get(&method).ok_or(ApiError::not_found(method))?;
                let state: &fedimint_walletv2_server::Wallet = consensus_api
                    .context(&request.request, Some(module_id))
                    .await;
                (endpoint.handler)(state, request.request).await
            }
            other => Err(ApiError::not_found(other.to_string())),
        },
    }
}
