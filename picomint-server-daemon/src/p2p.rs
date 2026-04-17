//! Federation-internal p2p: message types, iroh connector, and reconnecting
//! connection manager used by consensus / DKG.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context as _;
use async_channel::{Receiver, Sender, bounded};
use bitcoin::hashes::sha256;
use bls12_381::{G1Projective, G2Projective, Scalar};
use futures::FutureExt;
use futures::future::select_all;
use iroh::endpoint::{Connection, RecvStream};
use iroh::{Endpoint, PublicKey, SecretKey, Watcher as _};
use picomint_api_client::session_outcome::SignedSessionOutcome;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::module::PICOMINT_ALPN;
use picomint_core::net::STANDARD_PICOMINT_P2P_PORT;
use picomint_core::net::iroh::build_iroh_endpoint;
use picomint_core::task::{TaskGroup, sleep};
use picomint_core::util::backoff_util::{FibonacciBackoff, api_networking_backoff};
use picomint_core::util::{FmtCompactAnyhow, SafeUrl};
use picomint_core::{PeerId, secp256k1};
use picomint_logging::{LOG_CONSENSUS, LOG_NET_PEER};
use picomint_server_core::P2PConnectionStatus;
use tokio::sync::watch;
use tracing::{Instrument, debug, info, info_span, warn};

// ── P2P message types ───────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq, Encodable, Decodable)]
pub enum P2PMessage {
    Aleph(Vec<u8>),
    SessionSignature(secp256k1::schnorr::Signature),
    SessionIndex(u64),
    SignedSessionOutcome(SignedSessionOutcome),
    Checksum(sha256::Hash),
    DkgG1(DkgMessageG1),
    DkgG2(DkgMessageG2),
    Encodable(Vec<u8>),
}

#[derive(Debug, PartialEq, Eq, Clone, Encodable, Decodable)]
pub enum DkgMessageG1 {
    Hash(sha256::Hash),
    Commitment(Vec<G1Projective>),
    Share(Scalar),
}

#[derive(Debug, PartialEq, Eq, Clone, Encodable, Decodable)]
pub enum DkgMessageG2 {
    Hash(sha256::Hash),
    Commitment(Vec<G2Projective>),
    Share(Scalar),
}

// ── Connection primitives ───────────────────────────────────────────────────

/// Maximum size of a p2p message in bytes. The largest message we expect to
/// receive is a signed session outcome.
const MAX_P2P_MESSAGE_SIZE: usize = 10_000_000;

/// Parse the host and port from a p2p `picomint://` url.
pub fn parse_p2p(url: &SafeUrl) -> anyhow::Result<String> {
    anyhow::ensure!(url.scheme() == "picomint", "p2p url has invalid scheme");

    let host = url.host_str().context("p2p url is missing host")?;

    let port = url.port().unwrap_or(STANDARD_PICOMINT_P2P_PORT);

    Ok(format!("{host}:{port}"))
}

/// Thin wrapper over an iroh [`Connection`] that sends and receives
/// consensus-encoded p2p messages.
pub struct P2PConnection {
    connection: Connection,
}

impl P2PConnection {
    pub fn new(connection: Connection) -> Self {
        Self { connection }
    }

    /// Send a single message over a fresh uni stream. Not cancel-safe.
    pub async fn send<M: Encodable>(&self, message: M) -> anyhow::Result<()> {
        let mut sink = self.connection.open_uni().await?;

        sink.write_all(&message.consensus_encode_to_vec()).await?;

        sink.finish()?;

        Ok(())
    }

    /// Await the next incoming uni stream. Cancel-safe — no bytes are
    /// consumed here, so dropping this future before completion does not
    /// lose any message data. The returned [`RecvStream`] must then be
    /// read to completion with [`P2PConnection::read_frame`], outside of
    /// any `select!`, to preserve message ordering.
    pub async fn accept_stream(&self) -> anyhow::Result<RecvStream> {
        Ok(self.connection.accept_uni().await?)
    }

    /// Drain a uni stream previously returned by [`Self::accept_stream`]
    /// and decode it as `M`. Not cancel-safe — do not call inside
    /// `select!`.
    pub async fn read_frame<M: Decodable + Send + 'static>(
        stream: &mut RecvStream,
    ) -> anyhow::Result<M> {
        let bytes = stream.read_to_end(MAX_P2P_MESSAGE_SIZE).await?;

        Ok(M::consensus_decode_exact(&bytes)?)
    }

    pub fn rtt(&self) -> Option<Duration> {
        let paths = self.connection.paths();
        paths
            .peek()
            .iter()
            .find(|p| p.is_selected())
            .and_then(|p| p.rtt())
    }
}

/// Iroh-backed connector that dials peers by their pinned node id and accepts
/// incoming connections authenticated against the same node-id set.
#[derive(Clone)]
pub struct P2PConnector {
    node_ids: BTreeMap<PeerId, PublicKey>,
    endpoint: Endpoint,
}

impl P2PConnector {
    pub async fn new(
        secret_key: SecretKey,
        p2p_addr: SocketAddr,
        node_ids: BTreeMap<PeerId, PublicKey>,
    ) -> anyhow::Result<Self> {
        let identity = *node_ids
            .iter()
            .find(|entry| entry.1 == &secret_key.public())
            .expect("Our public key is not part of the keyset")
            .0;

        let endpoint = build_iroh_endpoint(secret_key, p2p_addr, PICOMINT_ALPN).await?;

        Ok(Self {
            node_ids: node_ids
                .into_iter()
                .filter(|entry| entry.0 != identity)
                .collect(),
            endpoint,
        })
    }

    pub fn peers(&self) -> Vec<PeerId> {
        self.node_ids.keys().copied().collect()
    }

    pub async fn connect(&self, peer: PeerId) -> anyhow::Result<P2PConnection> {
        let node_id = *self.node_ids.get(&peer).expect("No node id found for peer");

        let connection = self.endpoint.connect(node_id, PICOMINT_ALPN).await?;

        Ok(P2PConnection::new(connection))
    }

    /// Accept the next incoming connection, fully completing the QUIC
    /// handshake. The remote node-id is compared against the pinned peer set:
    /// a match produces [`Accepted::Peer`] for the federation-internal p2p
    /// path; anything else is [`Accepted::Foreign`] for the public API path
    /// (one endpoint, two logical consumers demuxed here by node-id).
    pub async fn accept(&self) -> anyhow::Result<Accepted> {
        let connection = self
            .endpoint
            .accept()
            .await
            .context("Listener closed unexpectedly")?
            .accept()?
            .await?;

        let node_id = connection.remote_id();

        for (peer, pk) in &self.node_ids {
            if *pk == node_id {
                return Ok(Accepted::Peer(*peer, P2PConnection::new(connection)));
            }
        }

        Ok(Accepted::Foreign(connection))
    }
}

/// Result of [`P2PConnector::accept`]: either a federation peer (pinned
/// node-id) or a foreign connection (public-API client).
pub enum Accepted {
    Peer(PeerId, P2PConnection),
    Foreign(Connection),
}

// ── Connection manager ──────────────────────────────────────────────────────

pub type P2PStatusSenders = BTreeMap<PeerId, watch::Sender<Option<P2PConnectionStatus>>>;
pub type P2PStatusReceivers = BTreeMap<PeerId, watch::Receiver<Option<P2PConnectionStatus>>>;

/// This enum defines the intended recipient of a p2p message.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    Everyone,
    Peer(PeerId),
}

pub fn p2p_status_channels(peers: Vec<PeerId>) -> (P2PStatusSenders, P2PStatusReceivers) {
    let mut senders = BTreeMap::new();
    let mut receivers = BTreeMap::new();

    for peer in peers {
        let (sender, receiver) = watch::channel(None);

        senders.insert(peer, sender);
        receivers.insert(peer, receiver);
    }

    (senders, receivers)
}

/// Connection manager that tries to keep iroh connections open to all peers
/// and exchanges consensus-encoded messages of type `M` with them.
#[derive(Clone)]
pub struct ReconnectP2PConnections<M> {
    connections: BTreeMap<PeerId, PeerChannel<M>>,
}

impl<M: Encodable + Decodable + Clone + Send + 'static> ReconnectP2PConnections<M> {
    pub fn new(
        identity: PeerId,
        connector: P2PConnector,
        task_group: &TaskGroup,
        status_senders: P2PStatusSenders,
        foreign_conn_tx: Sender<Connection>,
    ) -> Self {
        let mut connection_senders = BTreeMap::new();
        let mut connections = BTreeMap::new();

        for peer_id in connector.peers() {
            assert_ne!(peer_id, identity);

            let (connection_sender, connection_receiver) = bounded(4);

            let connection = PeerChannel::new(
                identity,
                peer_id,
                connector.clone(),
                connection_receiver,
                status_senders
                    .get(&peer_id)
                    .expect("No p2p status sender for peer")
                    .clone(),
                task_group,
            );

            connection_senders.insert(peer_id, connection_sender);
            connections.insert(peer_id, connection);
        }

        task_group.spawn_cancellable("handle-incoming-p2p-connections", async move {
            info!(target: LOG_NET_PEER, "Starting listening task for p2p connections");

            loop {
                match connector.accept().await {
                    Ok(Accepted::Peer(peer, connection)) => {
                        if connection_senders
                            .get_mut(&peer)
                            .expect("Authenticating connectors dont return unknown peers")
                            .send(connection)
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    Ok(Accepted::Foreign(connection)) => {
                        // Public API client. Drop on backpressure — the
                        // api-layer consumer isn't running yet during DKG and
                        // a pre-bootstrap client has no business connecting.
                        if foreign_conn_tx.try_send(connection).is_err() {
                            debug!(
                                target: LOG_NET_PEER,
                                "Dropping foreign connection: api channel full or closed"
                            );
                        }
                    }
                    Err(err) => {
                        warn!(target: LOG_NET_PEER, our_id = %identity, err = %err.fmt_compact_anyhow(), "Error while opening incoming connection");
                    }
                }
            }

            info!(target: LOG_NET_PEER, "Shutting down listening task for p2p connections");
        });

        ReconnectP2PConnections { connections }
    }

    /// Send `message` to `recipient`. Drops the message if the outgoing
    /// channel is full (the consensus layer is expected to resend).
    pub fn send(&self, recipient: Recipient, message: M) {
        match recipient {
            Recipient::Everyone => {
                for connection in self.connections.values() {
                    connection.try_send(message.clone());
                }
            }
            Recipient::Peer(peer) => match self.connections.get(&peer) {
                Some(connection) => {
                    connection.try_send(message);
                }
                _ => {
                    warn!(target: LOG_NET_PEER, "No connection for peer {peer}");
                }
            },
        }
    }

    /// Await the next message from any peer; `None` when shutting down.
    pub async fn receive(&self) -> Option<(PeerId, M)> {
        select_all(self.connections.iter().map(|(&peer, connection)| {
            Box::pin(connection.receive().map(move |m| m.map(|m| (peer, m))))
        }))
        .await
        .0
    }

    /// Await the next message from `peer`; `None` when shutting down.
    pub async fn receive_from_peer(&self, peer: PeerId) -> Option<M> {
        self.connections
            .get(&peer)
            .expect("No connection found for peer")
            .receive()
            .await
    }
}

/// Per-peer outgoing queue and incoming queue, backed by a background state
/// machine that (re)establishes the underlying iroh connection.
#[derive(Clone)]
struct PeerChannel<M> {
    outgoing_sender: Sender<M>,
    incoming_receiver: Receiver<M>,
}

impl<M: Encodable + Decodable + Send + 'static> PeerChannel<M> {
    fn new(
        our_id: PeerId,
        peer_id: PeerId,
        connector: P2PConnector,
        incoming_connections: Receiver<P2PConnection>,
        status_sender: watch::Sender<Option<P2PConnectionStatus>>,
        task_group: &TaskGroup,
    ) -> Self {
        // Small message queues to avoid outdated messages such as requests for
        // signed session outcomes queueing up while a peer is disconnected;
        // the consensus layer is designed for an unreliable network and
        // re-sends as needed. During DKG we never have more than two messages
        // in these channels at once.
        let (outgoing_sender, outgoing_receiver) = bounded(5);
        let (incoming_sender, incoming_receiver) = bounded(5);

        task_group.spawn_cancellable(
            format!("io-state-machine-{peer_id}"),
            async move {
                info!(target: LOG_NET_PEER, "Starting peer connection state machine");

                let mut state_machine = P2PConnectionStateMachine {
                    common: P2PConnectionSMCommon {
                        incoming_sender,
                        outgoing_receiver,
                        our_id,
                        peer_id,
                        connector,
                        incoming_connections,
                        status_sender,
                    },
                    state: P2PConnectionSMState::Disconnected(api_networking_backoff()),
                };

                while let Some(sm) = state_machine.state_transition().await {
                    state_machine = sm;
                }

                info!(target: LOG_NET_PEER, "Shutting down peer connection state machine");
            }
            .instrument(info_span!("io-state-machine", ?peer_id)),
        );

        PeerChannel {
            outgoing_sender,
            incoming_receiver,
        }
    }

    fn try_send(&self, message: M) {
        if self.outgoing_sender.try_send(message).is_err() {
            debug!(target: LOG_NET_PEER, "Outgoing message channel is full");
        }
    }

    async fn receive(&self) -> Option<M> {
        self.incoming_receiver.recv().await.ok()
    }
}

struct P2PConnectionStateMachine<M> {
    state: P2PConnectionSMState,
    common: P2PConnectionSMCommon<M>,
}

struct P2PConnectionSMCommon<M> {
    incoming_sender: async_channel::Sender<M>,
    outgoing_receiver: async_channel::Receiver<M>,
    our_id: PeerId,
    peer_id: PeerId,
    connector: P2PConnector,
    incoming_connections: Receiver<P2PConnection>,
    status_sender: watch::Sender<Option<P2PConnectionStatus>>,
}

enum P2PConnectionSMState {
    Disconnected(FibonacciBackoff),
    Connected(P2PConnection),
}

impl<M: Encodable + Decodable + Send + 'static> P2PConnectionStateMachine<M> {
    async fn state_transition(mut self) -> Option<Self> {
        match self.state {
            P2PConnectionSMState::Disconnected(backoff) => {
                self.common.status_sender.send_replace(None);

                self.common.transition_disconnected(backoff).await
            }
            P2PConnectionSMState::Connected(connection) => {
                let status = P2PConnectionStatus {
                    rtt: connection.rtt(),
                };

                self.common.status_sender.send_replace(Some(status));

                self.common.transition_connected(connection).await
            }
        }
        .map(|state| P2PConnectionStateMachine {
            common: self.common,
            state,
        })
    }
}

impl<M: Encodable + Decodable + Send + 'static> P2PConnectionSMCommon<M> {
    async fn transition_connected(
        &mut self,
        connection: P2PConnection,
    ) -> Option<P2PConnectionSMState> {
        tokio::select! {
            message = self.outgoing_receiver.recv() => {
                Some(self.send_message(connection, message.ok()?).await)
            },
            connection = self.incoming_connections.recv() => {
                info!(target: LOG_NET_PEER, "Connected to peer");

                Some(P2PConnectionSMState::Connected(connection.ok()?))
            },
            stream = connection.accept_stream() => {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => return Some(self.disconnect(e)),
                };

                match P2PConnection::read_frame::<M>(&mut stream).await {
                    Ok(message) => {
                        if self.incoming_sender.try_send(message).is_err() {
                            debug!(target: LOG_NET_PEER, "Incoming message channel is full");
                        }

                        Some(P2PConnectionSMState::Connected(connection))
                    },
                    Err(e) => Some(self.disconnect(e)),
                }
            },
        }
    }

    fn disconnect(&self, error: anyhow::Error) -> P2PConnectionSMState {
        info!(target: LOG_NET_PEER, "Disconnected from peer: {}", error);

        P2PConnectionSMState::Disconnected(api_networking_backoff())
    }

    async fn send_message(
        &mut self,
        connection: P2PConnection,
        peer_message: M,
    ) -> P2PConnectionSMState {
        if let Err(e) = connection.send(peer_message).await {
            return self.disconnect(e);
        }

        P2PConnectionSMState::Connected(connection)
    }

    async fn transition_disconnected(
        &mut self,
        mut backoff: FibonacciBackoff,
    ) -> Option<P2PConnectionSMState> {
        tokio::select! {
            connection = self.incoming_connections.recv() => {
                info!(target: LOG_NET_PEER, "Connected to peer");

                Some(P2PConnectionSMState::Connected(connection.ok()?))
            },
            // To prevent reconnection ping-pongs, only the side with lower
            // PeerId reconnects.
            () = sleep(backoff.next().expect("Unlimited retries")), if self.our_id < self.peer_id => {
                info!(target: LOG_NET_PEER, "Attempting to reconnect to peer");

                match self.connector.connect(self.peer_id).await {
                    Ok(connection) => {
                        info!(target: LOG_NET_PEER, "Connected to peer");

                        return Some(P2PConnectionSMState::Connected(connection));
                    }
                    Err(e) => warn!(target: LOG_CONSENSUS, "Failed to connect to peer: {e}"),
                }

                Some(P2PConnectionSMState::Disconnected(backoff))
            },
        }
    }
}
