//! Connection infrastructure for fedimint API clients

use std::collections::{BTreeSet, HashMap};
use std::fmt::Debug;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT;
use fedimint_core::module::{
    ApiError, ApiMethod, ApiRequestErased, FEDIMINT_API_ALPN, IrohApiRequest,
};
use fedimint_core::util::SafeUrl;
use fedimint_core::util::backoff_util::{FibonacciBackoff, custom_backoff};
use fedimint_core::{PeerId, apply, async_trait_maybe_send};
use fedimint_logging::LOG_CLIENT_NET_API;
use iroh::endpoint::Connection;
use iroh::{Endpoint, PublicKey};
use serde_json::Value;
use thiserror::Error;
use tokio::sync::{OnceCell, broadcast, watch};
use tracing::{trace, warn};

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

#[apply(async_trait_maybe_send!)]
pub trait IConnection: Debug + Send + Sync + 'static {
    fn is_connected(&self) -> bool;
    async fn await_disconnection(&self);
}

pub type DynGuaridianConnection = Arc<dyn IGuardianConnection>;

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

    active_connections: watch::Sender<BTreeSet<SafeUrl>>,

    #[allow(clippy::type_complexity)]
    connections:
        Arc<tokio::sync::Mutex<HashMap<SafeUrl, Arc<ConnectionState<dyn IGuardianConnection>>>>>,
}

impl Clone for ConnectionPool {
    fn clone(&self) -> Self {
        Self {
            endpoint: self.endpoint.clone(),
            connections: self.connections.clone(),
            active_connections: self.active_connections.clone(),
        }
    }
}

impl ConnectionPool {
    pub fn new(endpoint: Endpoint) -> Self {
        Self {
            endpoint,
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

        let conn = pool_entry_arc
            .connection
            .get_or_try_init(|| async {
                let retry_delay = pool_entry_arc.pre_reconnect_delay();
                fedimint_core::runtime::sleep(retry_delay).await;

                trace!(target: LOG_CLIENT_NET_API, %url, "Attempting to create a new connection");
                let res = connect_guardian(&endpoint, url).await;

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

const IROH_MAX_RESPONSE_BYTES: usize = ALEPH_BFT_UNIT_BYTE_LIMIT * 3600 * 4 * 2;

/// Parse a node ID from an iroh:// URL.
pub fn node_id_from_url(url: &SafeUrl) -> anyhow::Result<PublicKey> {
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
) -> ServerResult<DynGuaridianConnection> {
    let node_id = node_id_from_url(url).map_err(|source| ServerError::InvalidPeerUrl {
        source,
        url: url.to_owned(),
    })?;

    let conn = endpoint
        .connect(node_id, FEDIMINT_API_ALPN)
        .await
        .map_err(|e| ServerError::Connection(e.into()))?;

    Ok(IGuardianConnection::into_dyn(conn))
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
