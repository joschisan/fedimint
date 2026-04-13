//! Connection infrastructure for fedimint API clients
//!
//! Provides connection pooling, error types, and iroh-based guardian
//! connections.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt::Debug;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT;
use fedimint_core::envs::{
    FM_IROH_N0_DISCOVERY_ENABLE_ENV, FM_IROH_PKARR_RESOLVER_ENABLE_ENV, is_env_var_set_opt,
    parse_kv_list_from_env,
};
use fedimint_core::module::{
    ApiError, ApiMethod, ApiRequestErased, FEDIMINT_API_ALPN, IrohApiRequest,
};
use fedimint_core::util::SafeUrl;
use fedimint_core::util::backoff_util::{FibonacciBackoff, custom_backoff};
use fedimint_core::{PeerId, apply, async_trait_maybe_send};
use fedimint_logging::{LOG_CLIENT_NET_API, LOG_NET_IROH};
use iroh::discovery::pkarr::PkarrResolver;
use iroh::endpoint::Connection;
use iroh::{Endpoint, NodeAddr, NodeId, PublicKey};
use iroh_base::ticket::NodeTicket;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{OnceCell, broadcast, watch};
use tracing::{debug, trace, warn};

// ── Error types ─────────────────────────────────────────────────────────────

/// An API request error when calling a single federation peer
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    #[error("Response deserialization error: {0}")]
    ResponseDeserialization(anyhow::Error),

    #[error("Invalid peer id: {peer_id}")]
    InvalidPeerId { peer_id: PeerId },

    #[error("Invalid peer url: {url}")]
    InvalidPeerUrl { url: SafeUrl, source: anyhow::Error },

    #[error("Invalid endpoint")]
    InvalidEndpoint(anyhow::Error),

    #[error("Connection failed: {0}")]
    Connection(anyhow::Error),

    #[error("Transport error: {0}")]
    Transport(anyhow::Error),

    #[error("Invalid rpc id")]
    InvalidRpcId(anyhow::Error),

    #[error("Invalid request")]
    InvalidRequest(anyhow::Error),

    #[error("Invalid response: {0}")]
    InvalidResponse(anyhow::Error),

    #[error("Unspecified server error: {0}")]
    ServerError(anyhow::Error),

    #[error("Unspecified condition error: {0}")]
    ConditionFailed(anyhow::Error),

    #[error("Unspecified internal client error: {0}")]
    InternalClientError(anyhow::Error),
}

impl ServerError {
    pub fn is_unusual(&self) -> bool {
        match self {
            ServerError::ResponseDeserialization(_)
            | ServerError::InvalidPeerId { .. }
            | ServerError::InvalidPeerUrl { .. }
            | ServerError::InvalidResponse(_)
            | ServerError::InvalidRpcId(_)
            | ServerError::InvalidRequest(_)
            | ServerError::InternalClientError(_)
            | ServerError::InvalidEndpoint(_)
            | ServerError::ServerError(_) => true,
            ServerError::Connection(_)
            | ServerError::Transport(_)
            | ServerError::ConditionFailed(_) => false,
        }
    }

    pub fn report_if_unusual(&self, peer_id: PeerId, context: &str) {
        let unusual = self.is_unusual();

        trace!(target: LOG_CLIENT_NET_API, error = %self, %context, "ServerError");

        if unusual {
            warn!(target: LOG_CLIENT_NET_API, error = %self, %context, %peer_id, "Unusual ServerError");
        }
    }
}

pub type ServerResult<T> = Result<T, ServerError>;

// ── Connection traits ───────────────────────────────────────────────────────

/// Generic connection trait
#[apply(async_trait_maybe_send!)]
pub trait IConnection: Debug + Send + Sync + 'static {
    fn is_connected(&self) -> bool;
    async fn await_disconnection(&self);
}

/// A connection from api client to a federation guardian (type erased)
pub type DynGuaridianConnection = Arc<dyn IGuardianConnection>;

/// A connection from api client to a federation guardian
#[async_trait::async_trait]
pub trait IGuardianConnection: IConnection + Debug + Send + Sync + 'static {
    async fn request(&self, method: ApiMethod, request: ApiRequestErased) -> ServerResult<Value>;

    fn into_dyn(self) -> DynGuaridianConnection
    where
        Self: Sized,
    {
        Arc::new(self)
    }
}

// ── Connection pool ─────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ConnectionPool {
    endpoint: Endpoint,
    connection_overrides: BTreeMap<NodeId, NodeAddr>,

    active_connections: watch::Sender<BTreeSet<SafeUrl>>,

    #[allow(clippy::type_complexity)]
    connections:
        Arc<tokio::sync::Mutex<HashMap<SafeUrl, Arc<ConnectionState<dyn IGuardianConnection>>>>>,
}

impl Clone for ConnectionPool {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            connection_overrides: self.connection_overrides.clone(),
            connections: self.connections.clone(),
            active_connections: self.active_connections.clone(),
        }
    }
}

impl ConnectionPool {
    pub fn new(endpoint: Endpoint, connection_overrides: BTreeMap<NodeId, NodeAddr>) -> Self {
        Self {
            endpoint,
            connection_overrides,
            connections: Arc::new(tokio::sync::Mutex::new(HashMap::new())),
            active_connections: watch::channel(BTreeSet::new()).0,
        }
    }

    async fn get_or_init_pool_entry(
        &self,
        url: &SafeUrl,
    ) -> Arc<ConnectionState<dyn IGuardianConnection>> {
        let mut pool_locked = self.connections.lock().await;
        pool_locked
            .entry(url.to_owned())
            .and_modify(|entry_arc| {
                if let Some(existing_conn) = entry_arc.connection.get()
                    && !existing_conn.is_connected()
                {
                    trace!(
                        target: LOG_CLIENT_NET_API,
                        %url,
                        "Existing connection is disconnected, removing from pool"
                    );
                    self.active_connections.send_modify(|v| {
                        v.remove(url);
                    });
                    *entry_arc = Arc::new(ConnectionState::new_reconnecting());
                }
            })
            .or_insert_with(|| Arc::new(ConnectionState::new_initial()))
            .clone()
    }

    pub async fn get_or_create_connection(
        &self,
        url: &SafeUrl,
    ) -> ServerResult<DynGuaridianConnection> {
        let pool_entry_arc = self.get_or_init_pool_entry(url).await;

        let leader_tx = loop {
            let mut leader_rx = {
                let mut chan_locked = pool_entry_arc
                    .merge_connection_attempts_chan
                    .lock()
                    .expect("locking error");

                if chan_locked.is_closed() {
                    let (leader_tx, leader_rx) = broadcast::channel(1);
                    *chan_locked = leader_rx;
                    break leader_tx;
                }

                chan_locked.resubscribe()
            };

            if let Ok(res) = leader_rx.recv().await {
                match res {
                    Ok(o) => return Ok(o),
                    Err(err) => {
                        return Err(ServerError::Connection(anyhow::format_err!("{}", err)));
                    }
                }
            }
        };

        let endpoint = self.endpoint.clone();
        let overrides = self.connection_overrides.clone();

        let conn = pool_entry_arc
            .connection
            .get_or_try_init(|| async {
                let retry_delay = pool_entry_arc.pre_reconnect_delay();
                fedimint_core::runtime::sleep(retry_delay).await;

                trace!(target: LOG_CLIENT_NET_API, %url, "Attempting to create a new connection");
                let res = connect_guardian(&endpoint, url, &overrides).await;

                let _ = leader_tx.send(
                    res.as_ref()
                        .map(|o| o.clone())
                        .map_err(|err| err.to_string()),
                );

                let conn = res?;

                self.active_connections.send_modify(|v| {
                    v.insert(url.clone());
                });

                fedimint_core::runtime::spawn("connection disconnect watch", {
                    let conn = conn.clone();
                    let s = self.clone();
                    let url = url.clone();
                    async move {
                        conn.await_disconnection().await;
                        s.get_or_init_pool_entry(&url).await;
                    }
                });

                Ok(conn)
            })
            .await?;

        trace!(target: LOG_CLIENT_NET_API, %url, "Connection ready");
        Ok(conn.clone())
    }

    pub fn get_active_connection_receiver(&self) -> watch::Receiver<BTreeSet<SafeUrl>> {
        self.active_connections.subscribe()
    }
}

#[derive(Debug)]
struct ConnectionStateInner {
    fresh: bool,
    backoff: FibonacciBackoff,
}

#[derive(Debug)]
pub struct ConnectionState<T: ?Sized> {
    pub connection: OnceCell<Arc<T>>,
    merge_connection_attempts_chan:
        std::sync::Mutex<broadcast::Receiver<std::result::Result<Arc<T>, String>>>,
    inner: std::sync::Mutex<ConnectionStateInner>,
}

impl<T: ?Sized> ConnectionState<T> {
    pub fn new_initial() -> Self {
        Self {
            connection: OnceCell::new(),
            inner: std::sync::Mutex::new(ConnectionStateInner {
                fresh: true,
                backoff: custom_backoff(Duration::from_millis(5), Duration::from_secs(30), None),
            }),
            merge_connection_attempts_chan: std::sync::Mutex::new(broadcast::channel(1).1),
        }
    }

    pub fn new_reconnecting() -> Self {
        Self {
            connection: OnceCell::new(),
            inner: std::sync::Mutex::new(ConnectionStateInner {
                fresh: false,
                backoff: custom_backoff(Duration::from_millis(500), Duration::from_secs(30), None),
            }),
            merge_connection_attempts_chan: std::sync::Mutex::new(broadcast::channel(1).1),
        }
    }

    pub fn pre_reconnect_delay(&self) -> Duration {
        let mut backoff_locked = self.inner.lock().expect("Locking failed");
        let fresh = backoff_locked.fresh;
        backoff_locked.fresh = false;
        if fresh {
            Duration::default()
        } else {
            backoff_locked.backoff.next().expect("Keeps retrying")
        }
    }
}

// ── Iroh helpers ────────────────────────────────────────────────────────────

/// The maximum number of bytes we are willing to buffer when reading an API
/// response from an iroh QUIC stream.
const IROH_MAX_RESPONSE_BYTES: usize = ALEPH_BFT_UNIT_BYTE_LIMIT * 3600 * 4 * 2;

/// Create an iroh endpoint with the given configuration.
pub async fn create_iroh_endpoint(
    iroh_dns: Option<SafeUrl>,
    enable_dht: bool,
) -> anyhow::Result<Endpoint> {
    let mut builder = Endpoint::builder();

    if let Some(iroh_dns) = iroh_dns.map(SafeUrl::to_unsafe) {
        builder = builder.add_discovery(|_| Some(PkarrResolver::new(iroh_dns)));
    }

    let mut builder = builder.relay_mode(iroh::RelayMode::Disabled);

    #[cfg(not(target_family = "wasm"))]
    if enable_dht {
        builder = builder.discovery_dht();
    }

    {
        if is_env_var_set_opt(FM_IROH_PKARR_RESOLVER_ENABLE_ENV).unwrap_or(true) {
            #[cfg(target_family = "wasm")]
            {
                builder = builder.add_discovery(move |_| Some(PkarrResolver::n0_dns()));
            }
        } else {
            warn!(target: LOG_NET_IROH, "Iroh pkarr resolver is disabled");
        }

        if is_env_var_set_opt(FM_IROH_N0_DISCOVERY_ENABLE_ENV).unwrap_or(true) {
            #[cfg(not(target_family = "wasm"))]
            {
                builder = builder
                    .add_discovery(move |_| Some(iroh::discovery::dns::DnsDiscovery::n0_dns()));
            }
        } else {
            warn!(target: LOG_NET_IROH, "Iroh n0 discovery is disabled");
        }
    }

    let endpoint = builder.bind().await?;
    debug!(
        target: LOG_NET_IROH,
        node_id = %endpoint.node_id(),
        node_id_pkarr = %z32::encode(endpoint.node_id().as_bytes()),
        "Iroh endpoint bound"
    );
    Ok(endpoint)
}

/// Load iroh connection overrides from environment variables.
pub fn load_iroh_connection_overrides() -> anyhow::Result<BTreeMap<NodeId, NodeAddr>> {
    const FM_IROH_CONNECT_OVERRIDES_ENV: &str = "FM_IROH_CONNECT_OVERRIDES";
    const FM_GW_IROH_CONNECT_OVERRIDES_ENV: &str = "FM_GW_IROH_CONNECT_OVERRIDES";

    let mut overrides = BTreeMap::new();
    for (k, v) in parse_kv_list_from_env::<_, NodeTicket>(FM_IROH_CONNECT_OVERRIDES_ENV)? {
        overrides.insert(k, v.into());
    }
    for (k, v) in parse_kv_list_from_env::<_, NodeTicket>(FM_GW_IROH_CONNECT_OVERRIDES_ENV)? {
        overrides.insert(k, v.into());
    }
    Ok(overrides)
}

/// Parse a node ID from an iroh:// URL.
pub fn node_id_from_url(url: &SafeUrl) -> anyhow::Result<NodeId> {
    if url.scheme() != "iroh" {
        bail!("Unsupported scheme: {}, expected iroh://", url.scheme());
    }
    let host = url.host_str().context("Missing host string in Iroh URL")?;
    PublicKey::from_str(host).context("Failed to parse node id")
}

/// Connect to a guardian via iroh.
pub async fn connect_guardian(
    endpoint: &Endpoint,
    url: &SafeUrl,
    connection_overrides: &BTreeMap<NodeId, NodeAddr>,
) -> ServerResult<DynGuaridianConnection> {
    let node_id = node_id_from_url(url).map_err(|source| ServerError::InvalidPeerUrl {
        source,
        url: url.to_owned(),
    })?;

    let conn = match connection_overrides.get(&node_id).cloned() {
        Some(node_addr) => {
            trace!(target: LOG_NET_IROH, %node_id, "Using a connectivity override for connection");
            let conn = endpoint.connect(node_addr, FEDIMINT_API_ALPN).await;

            #[cfg(not(target_family = "wasm"))]
            if conn.is_ok() {
                spawn_connection_monitoring(endpoint, node_id);
            }
            conn
        }
        None => endpoint.connect(node_id, FEDIMINT_API_ALPN).await,
    }
    .map_err(ServerError::Connection)?;

    Ok(IGuardianConnection::into_dyn(conn))
}

#[cfg(not(target_family = "wasm"))]
fn spawn_connection_monitoring(endpoint: &Endpoint, node_id: NodeId) {
    if let Ok(mut conn_type_watcher) = endpoint.conn_type(node_id) {
        #[allow(clippy::let_underscore_future)]
        let _ = fedimint_core::task::spawn("iroh connection", async move {
            if let Ok(conn_type) = conn_type_watcher.get() {
                debug!(target: LOG_NET_IROH, %node_id, type = %conn_type, "Connection type (initial)");
            }
            while let Ok(event) = conn_type_watcher.updated().await {
                debug!(target: LOG_NET_IROH, %node_id, type = %event, "Connection type (changed)");
            }
        });
    }
}

// ── Iroh IConnection / IGuardianConnection impls ────────────────────────────

#[apply(async_trait_maybe_send!)]
impl IConnection for Connection {
    async fn await_disconnection(&self) {
        self.closed().await;
    }

    fn is_connected(&self) -> bool {
        self.close_reason().is_none()
    }
}

#[async_trait::async_trait]
impl IGuardianConnection for Connection {
    async fn request(&self, method: ApiMethod, request: ApiRequestErased) -> ServerResult<Value> {
        let json = serde_json::to_vec(&IrohApiRequest { method, request })
            .expect("Serialization to vec can't fail");

        let (mut sink, mut stream) = self
            .open_bi()
            .await
            .map_err(|e| ServerError::Transport(e.into()))?;

        sink.write_all(&json)
            .await
            .map_err(|e| ServerError::Transport(e.into()))?;

        sink.finish()
            .map_err(|e| ServerError::Transport(e.into()))?;

        let response = stream
            .read_to_end(IROH_MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| ServerError::Transport(e.into()))?;

        let response = serde_json::from_slice::<Result<Value, ApiError>>(&response)
            .map_err(|e| ServerError::InvalidResponse(e.into()))?;

        response.map_err(|e| ServerError::InvalidResponse(anyhow::anyhow!("Api Error: {:?}", e)))
    }
}
