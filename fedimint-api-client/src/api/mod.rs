mod error;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::future::pending;
use std::pin::Pin;
use std::result;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context as _, anyhow, bail};
pub use error::{FederationError, OutputOutcomeError};
use fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT;
use fedimint_core::core::{Decoder, DynOutputOutcome, ModuleInstanceId, OutputOutcome};
use fedimint_core::endpoint_constants::{
    AWAIT_TRANSACTION_ENDPOINT, LIVENESS_ENDPOINT, SUBMIT_TRANSACTION_ENDPOINT,
};
use fedimint_core::module::{
    ApiError, ApiMethod, ApiRequestErased, FEDIMINT_API_ALPN, IrohApiRequest, SerdeModuleEncoding,
};
use fedimint_core::runtime::sleep;
use fedimint_core::task::{MaybeSend, MaybeSync};
use crate::transaction::{SerdeTransaction, Transaction, TransactionSubmissionOutcome};
use fedimint_core::util::backoff_util::api_networking_backoff;
use fedimint_core::util::{FmtCompact as _, SafeUrl};
use fedimint_core::{
    NumPeersExt, PeerId, TransactionId, apply, async_trait_maybe_send, dyn_newtype_define, util,
};
use fedimint_logging::LOG_CLIENT_NET_API;
use futures::stream::{BoxStream, FuturesUnordered};
use futures::{Future, StreamExt};
use iroh::endpoint::Connection;
use iroh::{Endpoint, PublicKey};
use jsonrpsee_core::DeserializeOwned;
use serde_json::Value;
use thiserror::Error;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tracing::{debug, instrument, trace, warn};

use crate::query::{QueryStep, QueryStrategy, ThresholdConsensus};

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

#[derive(Debug, Clone)]
enum PeerState {
    Connected(Connection),
    Disconnected,
}

pub type FederationResult<T> = Result<T, FederationError>;
pub type SerdeOutputOutcome = SerdeModuleEncoding<DynOutputOutcome>;

pub type OutputOutcomeResult<O> = result::Result<O, OutputOutcomeError>;

/// An API (module or global) that can query a federation
#[apply(async_trait_maybe_send!)]
pub trait IRawFederationApi: Debug + MaybeSend + MaybeSync {
    /// List of all federation peers for the purpose of iterating each peer
    /// in the federation.
    ///
    /// The underlying implementation is responsible for knowing how many
    /// and `PeerId`s of each. The caller of this interface most probably
    /// have some idea as well, but passing this set across every
    /// API call to the federation would be inconvenient.
    fn all_peers(&self) -> &BTreeSet<PeerId>;

    fn with_module(&self, id: ModuleInstanceId) -> DynModuleApi;

    /// Make request to a specific federation peer by `peer_id`
    async fn request_raw(
        &self,
        peer_id: PeerId,
        method: &str,
        params: &ApiRequestErased,
    ) -> ServerResult<Value>;

    /// Returns a stream of connection status for each peer
    ///
    /// The stream emits a new value whenever the connection status changes.
    fn connection_status_stream(&self) -> BoxStream<'static, BTreeMap<PeerId, bool>>;
}

/// An extension trait allowing to making federation-wide API call on top
/// [`IRawFederationApi`].
#[apply(async_trait_maybe_send!)]
pub trait FederationApiExt: IRawFederationApi {
    async fn request_single_peer<Ret>(
        &self,
        method: String,
        params: ApiRequestErased,
        peer: PeerId,
    ) -> ServerResult<Ret>
    where
        Ret: DeserializeOwned,
    {
        self.request_raw(peer, &method, &params)
            .await
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| ServerError::ResponseDeserialization(e.into()))
            })
    }

    async fn request_single_peer_federation<FedRet>(
        &self,
        method: String,
        params: ApiRequestErased,
        peer_id: PeerId,
    ) -> FederationResult<FedRet>
    where
        FedRet: serde::de::DeserializeOwned + Eq + Debug + Clone + MaybeSend,
    {
        self.request_raw(peer_id, &method, &params)
            .await
            .and_then(|v| {
                serde_json::from_value(v)
                    .map_err(|e| ServerError::ResponseDeserialization(e.into()))
            })
            .map_err(|e| error::FederationError::new_one_peer(peer_id, method, params, e))
    }

    /// Make an aggregate request to federation, using `strategy` to logically
    /// merge the responses.
    #[instrument(target = LOG_CLIENT_NET_API, skip_all, fields(method=method))]
    async fn request_with_strategy<PR: DeserializeOwned, FR: Debug>(
        &self,
        mut strategy: impl QueryStrategy<PR, FR> + MaybeSend,
        method: String,
        params: ApiRequestErased,
    ) -> FederationResult<FR> {
        // NOTE: `FuturesUnorderded` is a footgun, but all we do here is polling
        // completed results from it and we don't do any `await`s when
        // processing them, it should be totally OK.
        #[cfg(not(target_family = "wasm"))]
        let mut futures = FuturesUnordered::<Pin<Box<dyn Future<Output = _> + Send>>>::new();
        #[cfg(target_family = "wasm")]
        let mut futures = FuturesUnordered::<Pin<Box<dyn Future<Output = _>>>>::new();

        for peer in self.all_peers() {
            futures.push(Box::pin({
                let method = &method;
                let params = &params;
                async move {
                    let result = self
                        .request_single_peer(method.clone(), params.clone(), *peer)
                        .await;

                    (*peer, result)
                }
            }));
        }

        let mut peer_errors = BTreeMap::new();
        let peer_error_threshold = self.all_peers().to_num_peers().one_honest();

        loop {
            let (peer, result) = futures
                .next()
                .await
                .expect("Query strategy ran out of peers to query without returning a result");

            match result {
                Ok(response) => match strategy.process(peer, response) {
                    QueryStep::Retry(peers) => {
                        for peer in peers {
                            futures.push(Box::pin({
                                let method = &method;
                                let params = &params;
                                async move {
                                    let result = self
                                        .request_single_peer(method.clone(), params.clone(), peer)
                                        .await;

                                    (peer, result)
                                }
                            }));
                        }
                    }
                    QueryStep::Success(response) => return Ok(response),
                    QueryStep::Failure(e) => {
                        peer_errors.insert(peer, e);
                    }
                    QueryStep::Continue => {}
                },
                Err(e) => {
                    e.report_if_unusual(peer, "RequestWithStrategy");
                    peer_errors.insert(peer, e);
                }
            }

            if peer_errors.len() == peer_error_threshold {
                return Err(FederationError::peer_errors(
                    method.clone(),
                    params.params.clone(),
                    peer_errors,
                ));
            }
        }
    }

    #[instrument(target = LOG_CLIENT_NET_API, level = "debug", skip(self, strategy))]
    async fn request_with_strategy_retry<PR: DeserializeOwned + MaybeSend, FR: Debug>(
        &self,
        mut strategy: impl QueryStrategy<PR, FR> + MaybeSend,
        method: String,
        params: ApiRequestErased,
    ) -> FR {
        // NOTE: `FuturesUnorderded` is a footgun, but all we do here is polling
        // completed results from it and we don't do any `await`s when
        // processing them, it should be totally OK.
        #[cfg(not(target_family = "wasm"))]
        let mut futures = FuturesUnordered::<Pin<Box<dyn Future<Output = _> + Send>>>::new();
        #[cfg(target_family = "wasm")]
        let mut futures = FuturesUnordered::<Pin<Box<dyn Future<Output = _>>>>::new();

        for peer in self.all_peers() {
            futures.push(Box::pin({
                let method = &method;
                let params = &params;
                async move {
                    let response = util::retry(
                        format!("api-request-{method}-{peer}"),
                        api_networking_backoff(),
                        || async {
                            self.request_single_peer(method.clone(), params.clone(), *peer)
                                .await
                                .inspect_err(|e| {
                                    e.report_if_unusual(*peer, "QueryWithStrategyRetry");
                                })
                                .map_err(|e| anyhow!(e.to_string()))
                        },
                    )
                    .await
                    .expect("Number of retries has no limit");

                    (*peer, response)
                }
            }));
        }

        loop {
            let (peer, response) = match futures.next().await {
                Some(t) => t,
                None => pending().await,
            };

            match strategy.process(peer, response) {
                QueryStep::Retry(peers) => {
                    for peer in peers {
                        futures.push(Box::pin({
                            let method = &method;
                            let params = &params;
                            async move {
                                let response = util::retry(
                                    format!("api-request-{method}-{peer}"),
                                    api_networking_backoff(),
                                    || async {
                                        self.request_single_peer(
                                            method.clone(),
                                            params.clone(),
                                            peer,
                                        )
                                        .await
                                        .inspect_err(|err| {
                                            if err.is_unusual() {
                                                debug!(target: LOG_CLIENT_NET_API, err = %err.fmt_compact(), "Unusual peer error");
                                            }
                                        })
                                        .map_err(|e| anyhow!(e.to_string()))
                                    },
                                )
                                .await
                                .expect("Number of retries has no limit");

                                (peer, response)
                            }
                        }));
                    }
                }
                QueryStep::Success(response) => return response,
                QueryStep::Failure(e) => {
                    warn!(target: LOG_CLIENT_NET_API, "Query strategy returned non-retryable failure for peer {peer}: {e}");
                }
                QueryStep::Continue => {}
            }
        }
    }

    async fn request_current_consensus<Ret>(
        &self,
        method: String,
        params: ApiRequestErased,
    ) -> FederationResult<Ret>
    where
        Ret: DeserializeOwned + Eq + Debug + Clone + MaybeSend,
    {
        self.request_with_strategy(
            ThresholdConsensus::new(self.all_peers().to_num_peers()),
            method,
            params,
        )
        .await
    }

    async fn request_current_consensus_retry<Ret>(
        &self,
        method: String,
        params: ApiRequestErased,
    ) -> Ret
    where
        Ret: DeserializeOwned + Eq + Debug + Clone + MaybeSend,
    {
        self.request_with_strategy_retry(
            ThresholdConsensus::new(self.all_peers().to_num_peers()),
            method,
            params,
        )
        .await
    }

    async fn submit_transaction(
        &self,
        tx: Transaction,
    ) -> SerdeModuleEncoding<TransactionSubmissionOutcome> {
        self.request_current_consensus_retry(
            SUBMIT_TRANSACTION_ENDPOINT.to_owned(),
            ApiRequestErased::new(SerdeTransaction::from(&tx)),
        )
        .await
    }

    async fn await_transaction(&self, txid: TransactionId) -> TransactionId {
        self.request_current_consensus_retry(
            AWAIT_TRANSACTION_ENDPOINT.to_owned(),
            ApiRequestErased::new(txid),
        )
        .await
    }

    /// Lightweight liveness check — returns Ok(()) if the federation is
    /// reachable
    async fn liveness(&self) -> FederationResult<()> {
        self.request_current_consensus(LIVENESS_ENDPOINT.to_owned(), ApiRequestErased::default())
            .await
    }
}

#[apply(async_trait_maybe_send!)]
impl<T: ?Sized> FederationApiExt for T where T: IRawFederationApi {}

dyn_newtype_define! {
    #[derive(Clone)]
    pub DynModuleApi(Arc<IRawFederationApi>)
}

dyn_newtype_define! {
    #[derive(Clone)]
    pub DynGlobalApi(Arc<IRawFederationApi>)
}

impl DynGlobalApi {
    pub fn new(endpoint: Endpoint, peers: BTreeMap<PeerId, SafeUrl>) -> Self {
        FederationApi::new(endpoint, peers).into()
    }
}

pub fn deserialize_outcome<R>(
    outcome: &SerdeOutputOutcome,
    module_decoder: &Decoder,
) -> OutputOutcomeResult<R>
where
    R: OutputOutcome + MaybeSend,
{
    let dyn_outcome = outcome
        .try_into_inner_known_module_kind(module_decoder)
        .map_err(|e| OutputOutcomeError::ResponseDeserialization(e.into()))?;

    let source_instance = dyn_outcome.module_instance_id();

    dyn_outcome.as_any().downcast_ref().cloned().ok_or_else(|| {
        let target_type = std::any::type_name::<R>();
        OutputOutcomeError::ResponseDeserialization(anyhow!(
            "Could not downcast output outcome with instance id {source_instance} to {target_type}"
        ))
    })
}

/// Federation API client
///
/// Spawns a background task per peer at construction time that eagerly
/// connects and reconnects. Each task publishes its current [`PeerState`] on
/// a watch channel; requests wait for the first transition out of `None` and
/// read the live connection (or fail) from the current value.
#[derive(Clone, Debug)]
pub struct FederationApi {
    peers: BTreeMap<PeerId, SafeUrl>,
    peers_keys: BTreeSet<PeerId>,
    module_id: Option<ModuleInstanceId>,
    states: BTreeMap<PeerId, watch::Receiver<Option<PeerState>>>,
}

impl FederationApi {
    pub fn new(endpoint: Endpoint, peers: BTreeMap<PeerId, SafeUrl>) -> Self {
        let mut states = BTreeMap::new();

        for (peer_id, url) in &peers {
            let (tx, rx) = watch::channel(None);
            fedimint_core::runtime::spawn("fedimint-api-client-connection", {
                let endpoint = endpoint.clone();
                let url = url.clone();
                async move { connection_task(url, endpoint, tx).await }
            });
            states.insert(*peer_id, rx);
        }

        Self {
            peers_keys: peers.keys().copied().collect(),
            peers,
            module_id: None,
            states,
        }
    }

    async fn request(
        &self,
        peer: PeerId,
        method: ApiMethod,
        request: ApiRequestErased,
    ) -> ServerResult<Value> {
        trace!(target: LOG_CLIENT_NET_API, %peer, %method, "Api request");

        let mut rx = self
            .states
            .get(&peer)
            .ok_or(ServerError::InvalidPeerId { peer_id: peer })?
            .clone();

        let state = rx
            .wait_for(Option::is_some)
            .await
            .expect("connection task dropped")
            .clone()
            .expect("wait_for guarantees Some");

        let PeerState::Connected(conn) = state else {
            return Err(ServerError::Connection(anyhow!("peer not connected")));
        };

        let res = conn.request(method.clone(), request).await;

        trace!(target: LOG_CLIENT_NET_API, ?method, res_ok = res.is_ok(), "Api response");

        res
    }
}

async fn connection_task(
    url: SafeUrl,
    endpoint: Endpoint,
    state: watch::Sender<Option<PeerState>>,
) {
    let mut backoff = api_networking_backoff();

    loop {
        match connect_iroh(&endpoint, &url).await {
            Ok(conn) => {
                backoff = api_networking_backoff();

                let _ = state.send(Some(PeerState::Connected(conn.clone())));

                conn.closed().await;

                let _ = state.send(Some(PeerState::Disconnected));
            }
            Err(_) => {
                sleep(backoff.next().expect("Keeps retrying")).await;
            }
        }
    }
}

async fn connect_iroh(endpoint: &Endpoint, url: &SafeUrl) -> anyhow::Result<Connection> {
    let node_id = node_id_from_url(url)?;

    endpoint
        .connect(node_id, FEDIMINT_API_ALPN)
        .await
        .map_err(anyhow::Error::from)
}

/// Parse a node ID from an iroh:// URL.
fn node_id_from_url(url: &SafeUrl) -> anyhow::Result<PublicKey> {
    if url.scheme() != "iroh" {
        bail!("Unsupported scheme: {}, expected iroh://", url.scheme());
    }
    let host = url.host_str().context("Missing host string in Iroh URL")?;
    PublicKey::from_str(host).context("Failed to parse node id")
}

// ── Transport trait + iroh impl ─────────────────────────────────────────────

const IROH_MAX_RESPONSE_BYTES: usize = ALEPH_BFT_UNIT_BYTE_LIMIT * 3600 * 4 * 2;

#[async_trait::async_trait]
pub trait IGuardianConnection: Debug + Send + Sync + 'static {
    async fn request(&self, method: ApiMethod, request: ApiRequestErased) -> ServerResult<Value>;
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

#[apply(async_trait_maybe_send!)]
impl IRawFederationApi for FederationApi {
    fn all_peers(&self) -> &BTreeSet<PeerId> {
        &self.peers_keys
    }

    fn with_module(&self, id: ModuleInstanceId) -> DynModuleApi {
        FederationApi {
            peers: self.peers.clone(),
            peers_keys: self.peers_keys.clone(),
            module_id: Some(id),
            states: self.states.clone(),
        }
        .into()
    }

    #[instrument(
        target = LOG_CLIENT_NET_API,
        skip_all,
        fields(
            peer_id = %peer_id,
            method = %method,
            params = %params.params,
        )
    )]
    async fn request_raw(
        &self,
        peer_id: PeerId,
        method: &str,
        params: &ApiRequestErased,
    ) -> ServerResult<Value> {
        let method = match self.module_id {
            Some(module_id) => ApiMethod::Module(module_id, method.to_string()),
            None => ApiMethod::Core(method.to_string()),
        };

        self.request(peer_id, method, params.clone()).await
    }

    fn connection_status_stream(&self) -> BoxStream<'static, BTreeMap<PeerId, bool>> {
        let streams = self.states.iter().map(|(&peer, rx)| {
            WatchStream::new(rx.clone())
                .map(move |s| (peer, matches!(s, Some(PeerState::Connected(_)))))
        });

        let mut current = BTreeMap::new();
        futures::stream::select_all(streams)
            .map(move |(peer, connected)| {
                current.insert(peer, connected);
                current.clone()
            })
            .boxed()
    }
}

#[cfg(test)]
mod tests;
