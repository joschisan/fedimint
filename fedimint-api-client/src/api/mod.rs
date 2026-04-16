mod error;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::future::pending;
use std::pin::Pin;
use std::result;

use anyhow::anyhow;
pub use error::{FederationError, OutputOutcomeError};
use fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::endpoint_constants::{
    AWAIT_TRANSACTION_ENDPOINT, LIVENESS_ENDPOINT, SUBMIT_TRANSACTION_ENDPOINT,
};
use fedimint_core::module::{
    ApiError, ApiMethod, ApiRequestErased, FEDIMINT_API_ALPN, IrohApiRequest,
};
use fedimint_core::runtime::sleep;
use fedimint_core::task::MaybeSend;
use fedimint_core::util::FmtCompact as _;
use fedimint_core::util::backoff_util::api_networking_backoff;
use fedimint_core::{NumPeersExt, PeerId, TransactionId, util};
use fedimint_logging::LOG_CLIENT_NET_API;
use futures::stream::{BoxStream, FuturesUnordered};
use futures::{Future, StreamExt};
use iroh::endpoint::Connection;
use iroh::{Endpoint, PublicKey};
use thiserror::Error;
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tracing::{debug, instrument, trace, warn};

use crate::query::{QueryStep, QueryStrategy, ThresholdConsensus};
use crate::transaction::{Transaction, TransactionSubmissionOutcome};

// ── Error types ─────────────────────────────────────────────────────────────

/// An API request error when calling a single federation peer
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    #[error("Response deserialization error: {0}")]
    ResponseDeserialization(anyhow::Error),

    #[error("Invalid peer id: {peer_id}")]
    InvalidPeerId { peer_id: PeerId },

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

pub type OutputOutcomeResult<O> = result::Result<O, OutputOutcomeError>;

/// Federation API client.
///
/// Spawns a background task per peer at construction time that eagerly
/// connects and reconnects over iroh. Each task publishes its current
/// [`PeerState`] on a watch channel; requests wait for the first transition
/// out of `None` and read the live connection (or fail) from the current
/// value.
#[derive(Clone, Debug)]
pub struct FederationApi {
    peers: BTreeSet<PeerId>,
    module_id: Option<ModuleInstanceId>,
    states: BTreeMap<PeerId, watch::Receiver<Option<PeerState>>>,
}

impl FederationApi {
    pub fn new(endpoint: Endpoint, peers: BTreeMap<PeerId, PublicKey>) -> Self {
        let mut states = BTreeMap::new();

        for (peer_id, node_id) in &peers {
            let (tx, rx) = watch::channel(None);
            fedimint_core::runtime::spawn("fedimint-api-client-connection", {
                let endpoint = endpoint.clone();
                let node_id = *node_id;
                async move { connection_task(node_id, endpoint, tx).await }
            });
            states.insert(*peer_id, rx);
        }

        Self {
            peers: peers.keys().copied().collect(),
            module_id: None,
            states,
        }
    }

    /// List of all federation peers.
    pub fn all_peers(&self) -> &BTreeSet<PeerId> {
        &self.peers
    }

    /// Return a clone of this API scoped to a specific module, so subsequent
    /// calls dispatch to `ApiMethod::Module(module_id, ...)`.
    pub fn with_module(&self, id: ModuleInstanceId) -> FederationApi {
        FederationApi {
            peers: self.peers.clone(),
            module_id: Some(id),
            states: self.states.clone(),
        }
    }

    /// Stream of live connection status for each peer.
    pub fn connection_status_stream(&self) -> BoxStream<'static, BTreeMap<PeerId, bool>> {
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

    #[instrument(
        target = LOG_CLIENT_NET_API,
        skip_all,
        fields(peer_id = %peer_id, method = %method),
    )]
    pub async fn request_raw(
        &self,
        peer_id: PeerId,
        method: &str,
        params: &ApiRequestErased,
    ) -> ServerResult<Vec<u8>> {
        let method = match self.module_id {
            Some(module_id) => ApiMethod::Module(module_id, method.to_string()),
            None => ApiMethod::Core(method.to_string()),
        };

        trace!(target: LOG_CLIENT_NET_API, %peer_id, %method, "Api request");

        let mut rx = self
            .states
            .get(&peer_id)
            .ok_or(ServerError::InvalidPeerId { peer_id })?
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

        let res = request_over_connection(&conn, method.clone(), params.clone()).await;

        trace!(target: LOG_CLIENT_NET_API, ?method, res_ok = res.is_ok(), "Api response");

        res
    }

    pub async fn request_single_peer<Ret>(
        &self,
        method: String,
        params: ApiRequestErased,
        peer: PeerId,
    ) -> ServerResult<Ret>
    where
        Ret: Decodable,
    {
        self.request_raw(peer, &method, &params)
            .await
            .and_then(|bytes| {
                Ret::consensus_decode_exact(&bytes)
                    .map_err(|e| ServerError::ResponseDeserialization(e.into()))
            })
    }

    pub async fn request_single_peer_federation<FedRet>(
        &self,
        method: String,
        params: ApiRequestErased,
        peer_id: PeerId,
    ) -> FederationResult<FedRet>
    where
        FedRet: Decodable + Eq + Debug + Clone + MaybeSend,
    {
        self.request_raw(peer_id, &method, &params)
            .await
            .and_then(|bytes| {
                FedRet::consensus_decode_exact(&bytes)
                    .map_err(|e| ServerError::ResponseDeserialization(e.into()))
            })
            .map_err(|e| error::FederationError::new_one_peer(peer_id, method, params, e))
    }

    /// Make an aggregate request to federation, using `strategy` to logically
    /// merge the responses.
    #[instrument(target = LOG_CLIENT_NET_API, skip_all, fields(method = method))]
    pub async fn request_with_strategy<PR: Decodable, FR: Debug>(
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
                    params.clone(),
                    peer_errors,
                ));
            }
        }
    }

    #[instrument(target = LOG_CLIENT_NET_API, level = "debug", skip(self, strategy))]
    pub async fn request_with_strategy_retry<PR: Decodable + MaybeSend, FR: Debug>(
        &self,
        mut strategy: impl QueryStrategy<PR, FR> + MaybeSend,
        method: String,
        params: ApiRequestErased,
    ) -> FR {
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

    pub async fn request_current_consensus<Ret>(
        &self,
        method: String,
        params: ApiRequestErased,
    ) -> FederationResult<Ret>
    where
        Ret: Decodable + Eq + Debug + Clone + MaybeSend,
    {
        self.request_with_strategy(
            ThresholdConsensus::new(self.all_peers().to_num_peers()),
            method,
            params,
        )
        .await
    }

    pub async fn request_current_consensus_retry<Ret>(
        &self,
        method: String,
        params: ApiRequestErased,
    ) -> Ret
    where
        Ret: Decodable + Eq + Debug + Clone + MaybeSend,
    {
        self.request_with_strategy_retry(
            ThresholdConsensus::new(self.all_peers().to_num_peers()),
            method,
            params,
        )
        .await
    }

    pub async fn submit_transaction(&self, tx: Transaction) -> TransactionSubmissionOutcome {
        self.request_current_consensus_retry(
            SUBMIT_TRANSACTION_ENDPOINT.to_owned(),
            ApiRequestErased::new(tx),
        )
        .await
    }

    pub async fn await_transaction(&self, txid: TransactionId) -> TransactionId {
        self.request_current_consensus_retry(
            AWAIT_TRANSACTION_ENDPOINT.to_owned(),
            ApiRequestErased::new(txid),
        )
        .await
    }

    /// Lightweight liveness check — returns `Ok(())` if the federation is
    /// reachable.
    pub async fn liveness(&self) -> FederationResult<()> {
        self.request_current_consensus(LIVENESS_ENDPOINT.to_owned(), ApiRequestErased::default())
            .await
    }
}

async fn connection_task(
    node_id: PublicKey,
    endpoint: Endpoint,
    state: watch::Sender<Option<PeerState>>,
) {
    let mut backoff = api_networking_backoff();

    loop {
        match endpoint.connect(node_id, FEDIMINT_API_ALPN).await {
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

const IROH_MAX_RESPONSE_BYTES: usize = ALEPH_BFT_UNIT_BYTE_LIMIT * 3600 * 4 * 2;

async fn request_over_connection(
    connection: &Connection,
    method: ApiMethod,
    request: ApiRequestErased,
) -> ServerResult<Vec<u8>> {
    let request_bytes = IrohApiRequest { method, request }.consensus_encode_to_vec();

    let (mut sink, mut stream) = connection
        .open_bi()
        .await
        .map_err(|e| ServerError::Transport(e.into()))?;

    sink.write_all(&request_bytes)
        .await
        .map_err(|e| ServerError::Transport(e.into()))?;

    sink.finish()
        .map_err(|e| ServerError::Transport(e.into()))?;

    let response = stream
        .read_to_end(IROH_MAX_RESPONSE_BYTES)
        .await
        .map_err(|e| ServerError::Transport(e.into()))?;

    let response = <Result<Vec<u8>, ApiError>>::consensus_decode_exact(&response)
        .map_err(|e| ServerError::InvalidResponse(e.into()))?;

    response.map_err(|e| ServerError::InvalidResponse(anyhow::anyhow!("Api Error: {:?}", e)))
}

#[cfg(test)]
mod tests;
