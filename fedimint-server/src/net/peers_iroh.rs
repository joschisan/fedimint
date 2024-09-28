//! Implements a connection manager for communication with other federation
//! members
//!
//! The main interface is [`fedimint_core::net::peers::IPeerConnections`] and
//! its main implementation is [`ReconnectPeerConnections`], see these for
//! details.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use fedimint_api_client::api::PeerConnectionStatus;
use fedimint_core::task::{TaskGroup, TaskHandle};
use fedimint_core::PeerId;
use fedimint_logging::LOG_NET_PEER;
use futures::future::select_all;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use iroh_net::endpoint::{Connection, Endpoint, Incoming};
use iroh_net::key::SecretKey;
use iroh_net::{NodeAddr, NodeId};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{Receiver, Sender};
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio::time::Instant;
use tracing::{info, instrument, trace, warn};

use crate::consensus::aleph_bft::Recipient;
use crate::metrics::{PEER_CONNECT_COUNT, PEER_DISCONNECT_COUNT, PEER_MESSAGES_COUNT};

const FEDIMINT_P2P_ALPN: &[u8] = "FEDIMINT_P2P".as_bytes();

/// Try to establish a connection via all relays simultaneously and return the
/// first successful connection
async fn connect(endpoint: &Endpoint, node_id: NodeId) -> Option<Connection> {
    let relays = [
        iroh_net::defaults::prod::default_ap_relay_node(),
        iroh_net::defaults::prod::default_na_relay_node(),
        iroh_net::defaults::prod::default_eu_relay_node(),
    ];

    FuturesUnordered::from_iter(relays.into_iter().map(|relay| {
        endpoint.connect(
            NodeAddr::from_parts(node_id, Some(relay.url), Vec::new()),
            FEDIMINT_P2P_ALPN,
        )
    }))
    .filter_map(|connection| Box::pin(async move { connection.ok() }))
    .next()
    .await
}

/// Specifies the network configuration for federation-internal communication
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Our federation member's identity
    pub identity: PeerId,
    /// The secret key for our own Iroh Endpoint
    pub secret_key: SecretKey,
    /// Map of all peers' connection information we want to be connected to
    pub peers: BTreeMap<PeerId, NodeId>,
}

impl NetworkConfig {
    fn new(secret_key: SecretKey, peers: BTreeMap<PeerId, NodeId>) -> Self {
        let identity = peers
            .iter()
            .filter(|entry| entry.1 == &secret_key.public())
            .next()
            .expect("Our public key is not part of the keyset")
            .0
            .clone();

        Self {
            identity,
            secret_key,
            peers: peers
                .into_iter()
                .filter(|entry| entry.0 != identity)
                .collect(),
        }
    }
}

/// Connection manager that automatically reconnects to peers
#[derive(Clone)]
pub struct IrohPeerConnections {
    connections: HashMap<PeerId, PeerConnection>,
}

impl IrohPeerConnections {
    /// Creates a new `ReconnectPeerConnections` connection manager from a
    /// network config and a [`Connector`](crate::net::connect::Connector).
    /// See [`ReconnectPeerConnections`] for requirements on the
    /// `Connector`.
    #[instrument(skip_all)]
    pub(crate) async fn new(
        cfg: NetworkConfig,
        task_group: &TaskGroup,
        status_channels: Arc<RwLock<BTreeMap<PeerId, PeerConnectionStatus>>>,
    ) -> anyhow::Result<Self> {
        let endpoint = Endpoint::builder()
            .secret_key(cfg.secret_key.clone())
            .alpns(vec![FEDIMINT_P2P_ALPN.to_vec()])
            .bind()
            .await?;

        let mut connection_senders = HashMap::new();
        let mut connections = HashMap::new();

        for (peer_id, peer_node_id) in cfg.peers.iter() {
            assert_ne!(peer_id, &cfg.identity);

            let (connection_sender, connection_receiver) = mpsc::channel::<Connection>(32);

            let connection = PeerConnection::new(
                endpoint.clone(),
                cfg.identity,
                *peer_id,
                *peer_node_id,
                connection_receiver,
                status_channels.clone(),
                task_group,
            );

            connection_senders.insert(*peer_id, connection_sender);
            connections.insert(*peer_id, connection);

            status_channels
                .write()
                .await
                .insert(*peer_id, PeerConnectionStatus::Disconnected);
        }

        task_group.spawn("listen task", |handle| {
            Self::run_listen_task(cfg, endpoint, connection_senders, handle)
        });

        Ok(IrohPeerConnections { connections })
    }

    async fn run_listen_task(
        cfg: NetworkConfig,
        endpoint: Endpoint,
        mut senders: HashMap<PeerId, Sender<Connection>>,
        task_handle: TaskHandle,
    ) {
        let mut shutdown_rx = task_handle.make_shutdown_rx();

        while !task_handle.is_shutting_down() {
            tokio::select! {
                incoming =  endpoint.accept() => {
                    match incoming {
                        Some(incoming) => {
                            if let Err(e) = Self::handle_connection(&cfg, incoming, &mut senders).await {
                                warn!("Failed to handle incoming connection {e}");
                            }
                        }
                        None => return,
                    }

                               },
                () = &mut shutdown_rx => { return },
            };
        }
    }

    async fn handle_connection(
        cfg: &NetworkConfig,
        incoming: Incoming,
        senders: &mut HashMap<PeerId, Sender<Connection>>,
    ) -> anyhow::Result<()> {
        let connection = incoming.accept()?.await?;

        let node_id = iroh_net::endpoint::get_remote_node_id(&connection)?;

        let peer_id = cfg
            .peers
            .iter()
            .find(|entry| entry.1 == &node_id)
            .context("NodeId is unknown")?
            .0;

        senders
            .get_mut(peer_id)
            .expect("Connection sender is missing")
            .send(connection)
            .await
            .context("Could not send incoming connection to peer io task")
    }

    pub fn send(&self, message: Vec<u8>, recipient: Recipient) {
        match recipient {
            Recipient::Everyone => {
                for connection in self.connections.values() {
                    connection.send(message.clone());
                }
            }
            Recipient::Peer(peer) => {
                if let Some(connection) = self.connections.get(&peer) {
                    connection.send(message);
                } else {
                    trace!(target: LOG_NET_PEER,peer = ?peer, "Can not send message to unknown peer");
                }
            }
        }
    }

    async fn receive(&mut self) -> (PeerId, Vec<u8>) {
        // if all peers banned (or just solo-federation), just hang here as there's
        // never going to be any message. This avoids panic on `select_all` with
        // no futures.
        if self.connections.is_empty() {
            std::future::pending::<()>().await;
        }

        let futures = self.connections.iter_mut().map(|(&peer, connection)| {
            Box::pin(async move {
                if let Some(message) = connection.receive().await {
                    return (peer, message);
                }

                std::future::pending::<(PeerId, Vec<u8>)>().await
            })
        });

        select_all(futures).await.0
    }
}

#[derive(Clone)]
struct PeerConnection {
    outgoing: async_channel::Sender<Vec<u8>>,
    incoming: async_channel::Receiver<Vec<u8>>,
}

impl PeerConnection {
    #[allow(clippy::too_many_arguments)]
    fn new(
        endpoint: Endpoint,
        our_id: PeerId,
        peer_id: PeerId,
        peer_node_id: NodeId,
        incoming_connections: Receiver<Connection>,
        status_channels: Arc<RwLock<BTreeMap<PeerId, PeerConnectionStatus>>>,
        task_group: &TaskGroup,
    ) -> PeerConnection {
        let (outgoing_sender, outgoing_receiver) = async_channel::bounded(1024);
        let (incoming_sender, incoming_receiver) = async_channel::bounded(1024);

        task_group.spawn(
            format!("io-thread-peer-{peer_id}"),
            move |handle| async move {
                Self::run_connection_state_machine(
                    endpoint,
                    incoming_sender,
                    outgoing_receiver,
                    our_id,
                    peer_id,
                    peer_node_id,
                    incoming_connections,
                    status_channels,
                    &handle,
                )
                .await;
            },
        );

        PeerConnection {
            outgoing: outgoing_sender,
            incoming: incoming_receiver,
        }
    }

    fn send(&self, message: Vec<u8>) {
        if self.outgoing.try_send(message).is_err() {
            warn!(target: LOG_NET_PEER, "Could not send outgoing message since the channel is full");
        }
    }

    async fn receive(&mut self) -> Option<Vec<u8>> {
        self.incoming.recv().await.ok()
    }

    #[allow(clippy::too_many_arguments)] // TODO: consider refactoring
    #[instrument(
        name = "peer_io_thread",
        target = "net::peer",
        skip_all,
        // `id` so it doesn't conflict with argument names otherwise will not be shown
        fields(id = %peer_id)
    )]
    async fn run_connection_state_machine(
        endpoint: Endpoint,
        incoming: async_channel::Sender<Vec<u8>>,
        outgoing: async_channel::Receiver<Vec<u8>>,
        our_id: PeerId,
        peer_id: PeerId,
        peer_node_id: NodeId,
        incoming_connections: Receiver<Connection>,
        status_channels: Arc<RwLock<BTreeMap<PeerId, PeerConnectionStatus>>>,
        task_handle: &TaskHandle,
    ) {
        info!(target: LOG_NET_PEER, "Starting connection state machine for peer {peer_id}");

        let mut state_machine = ConnectionSM {
            common: ConnectionSMCommon {
                endpoint,
                incoming,
                outgoing,
                our_id_str: our_id.to_string(),
                our_id,
                peer_id_str: peer_id.to_string(),
                peer_id,
                peer_node_id,
                incoming_connections,
                status_channels,
            },
            state: ConnectionSMState::Disconnected(ConnectionSMStateDisconnected {
                reconnect_at: Instant::now(),
                reconnect_counter: 0,
            }),
        };

        while !task_handle.is_shutting_down() {
            if let Some(new_state) = state_machine.state_transition(task_handle).await {
                state_machine = new_state;
            } else {
                break;
            }
        }

        info!(target: LOG_NET_PEER, "Shutting down connection state machine for peer {peer_id}");
    }
}

struct ConnectionSM {
    common: ConnectionSMCommon,
    state: ConnectionSMState,
}

struct ConnectionSMCommon {
    endpoint: Endpoint,
    incoming: async_channel::Sender<Vec<u8>>,
    outgoing: async_channel::Receiver<Vec<u8>>,
    our_id: PeerId,
    our_id_str: String,
    peer_id: PeerId,
    peer_id_str: String,
    peer_node_id: NodeId,
    incoming_connections: Receiver<Connection>,
    status_channels: Arc<RwLock<BTreeMap<PeerId, PeerConnectionStatus>>>,
}

struct ConnectionSMStateDisconnected {
    reconnect_at: Instant,
    reconnect_counter: u64,
}

struct ConnectionSMStateConnected {
    connection: Connection,
}

enum ConnectionSMState {
    Disconnected(ConnectionSMStateDisconnected),
    Connected(ConnectionSMStateConnected),
}

impl ConnectionSM {
    async fn state_transition(mut self, task_handle: &TaskHandle) -> Option<Self> {
        match self.state {
            ConnectionSMState::Disconnected(disconnected) => {
                let state = self
                    .common
                    .state_transition_disconnected(disconnected, task_handle)
                    .await?;

                if let ConnectionSMState::Connected(..) = state {
                    self.common
                        .status_channels
                        .write()
                        .await
                        .insert(self.common.peer_id, PeerConnectionStatus::Connected);
                }

                Some(ConnectionSM {
                    common: self.common,
                    state,
                })
            }
            ConnectionSMState::Connected(connected) => {
                let state = self
                    .common
                    .state_transition_connected(connected, task_handle)
                    .await?;

                if let ConnectionSMState::Disconnected(..) = state {
                    self.common
                        .status_channels
                        .write()
                        .await
                        .insert(self.common.peer_id, PeerConnectionStatus::Disconnected);
                };

                Some(ConnectionSM {
                    common: self.common,
                    state,
                })
            }
        }
    }
}

impl ConnectionSMCommon {
    async fn state_transition_connected(
        &mut self,
        connected: ConnectionSMStateConnected,
        task_handle: &TaskHandle,
    ) -> Option<ConnectionSMState> {
        tokio::select! {
            message = self.outgoing.recv() => {
                match self.send_message(connected, message.ok()?).await {
                    Ok(connected) => Some(ConnectionSMState::Connected(connected)),
                    Err(e) => Some(self.disconnected(e)),
                }
            },
            connection = self.incoming_connections.recv() => {
                Some(self.connected(connection?))
            },
            stream = connected.connection.accept_uni() => {
                let mut stream = match stream {
                    Ok(stream) => stream,
                    Err(e) => return Some(self.disconnected(e.into())),
                };

               let message = match stream.read_to_end(100_000).await {
                    Ok(message) => message,
                    Err(e) => return Some(self.disconnected(e.into())),
                };

                PEER_MESSAGES_COUNT.with_label_values(&[&self.our_id_str, &self.peer_id_str, "incoming"]).inc();

                if self.incoming.try_send(message).is_err(){
                    warn!(target: LOG_NET_PEER, "Could not relay incoming message");
                }

                Some(ConnectionSMState::Connected(connected))
            },
            () = task_handle.make_shutdown_rx() => {
                None
            },
        }
    }

    fn connected(&mut self, connection: Connection) -> ConnectionSMState {
        info!("Peer {} is connected", self.peer_id);

        ConnectionSMState::Connected(ConnectionSMStateConnected { connection })
    }

    fn disconnected(&self, error: anyhow::Error) -> ConnectionSMState {
        info!(target: LOG_NET_PEER, "Peer {} is disconnected: {}", self.peer_id, error);

        PEER_DISCONNECT_COUNT
            .with_label_values(&[&self.our_id_str, &self.peer_id_str])
            .inc();

        ConnectionSMState::Disconnected(ConnectionSMStateDisconnected {
            reconnect_at: Instant::now(),
            reconnect_counter: 0,
        })
    }

    async fn send_message(
        &self,
        connected: ConnectionSMStateConnected,
        message: Vec<u8>,
    ) -> anyhow::Result<ConnectionSMStateConnected> {
        PEER_MESSAGES_COUNT
            .with_label_values(&[&self.our_id_str, &self.peer_id_str, "outgoing"])
            .inc();

        let mut sink = connected.connection.open_uni().await?;

        sink.write_all(&message).await?;

        sink.finish()?;

        Ok(connected)
    }

    async fn state_transition_disconnected(
        &mut self,
        disconnected: ConnectionSMStateDisconnected,
        task_handle: &TaskHandle,
    ) -> Option<ConnectionSMState> {
        tokio::select! {
            connection = self.incoming_connections.recv() => {
                let connection = connection?;

                PEER_CONNECT_COUNT.with_label_values(&[&self.our_id_str, &self.peer_id_str, "incoming"]).inc();

                Some(self.connected(connection))
            },
            // to prevent "reconnection ping-pongs", only the side with lower PeerId is responsible for reconnecting
            () = tokio::time::sleep_until(disconnected.reconnect_at), if self.our_id < self.peer_id => {
                Some(self.reconnect(disconnected).await)
            },
            () = task_handle.make_shutdown_rx() => {
                None
            },
        }
    }

    async fn reconnect(
        &mut self,
        disconnected: ConnectionSMStateDisconnected,
    ) -> ConnectionSMState {
        if let Some(remote_info) = self.endpoint.remote_info(self.peer_node_id) {
            let addr = NodeAddr::from_parts(
                self.peer_node_id,
                remote_info.relay_url.map(|relay_info| relay_info.relay_url),
                remote_info
                    .addrs
                    .into_iter()
                    .map(|addr_info| addr_info.addr)
                    .collect(),
            );

            if let Ok(connection) = self.endpoint.connect(addr, FEDIMINT_P2P_ALPN).await {
                return self.connected(connection);
            };
        }

        if let Some(connection) = connect(&self.endpoint, self.peer_node_id).await {
            return self.connected(connection);
        };

        ConnectionSMState::Disconnected(ConnectionSMStateDisconnected {
            reconnect_at: Instant::now() + Duration::from_secs(10),
            reconnect_counter: disconnected.reconnect_counter + 1,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use fedimint_core::task::{sleep, TaskGroup};
    use fedimint_core::PeerId;
    use iroh_net::key::SecretKey;
    use iroh_net::{Endpoint, NodeId};
    use tokio::sync::RwLock;

    use super::{IrohApiConnections, IrohPeerConnections, NetworkConfig, FEDIMINT_P2P_ALPN};
    use crate::consensus::aleph_bft::Recipient;

    #[tokio::test]
    async fn test_iroh_peer_connections() -> anyhow::Result<()> {
        let secret_keys = (0_u16..7)
            .map(|i| (PeerId::from(i), SecretKey::generate()))
            .collect::<BTreeMap<PeerId, SecretKey>>();

        let public_keys = secret_keys
            .iter()
            .map(|(peer_id, sk)| (*peer_id, sk.public()))
            .collect::<BTreeMap<PeerId, NodeId>>();

        let task_group = TaskGroup::new();
        let mut connections = BTreeMap::new();

        for (peer_id, sk) in secret_keys {
            let connection = IrohPeerConnections::new(
                NetworkConfig::new(sk, public_keys.clone()),
                &task_group,
                Arc::new(RwLock::new(BTreeMap::new())),
            )
            .await?;

            connections.insert(peer_id, connection);
        }

        for i in 0_u16..7 {
            let message = i.to_be_bytes().to_vec();

            connections
                .get(&PeerId::from(i))
                .expect("Peer {i} should exist")
                .send(message.clone(), Recipient::Everyone);

            for (receiver_id, connection) in &mut connections {
                if receiver_id != &PeerId::from(i) {
                    let (origin_id, msg) = connection.receive().await;

                    println!(
                        "Peer {:?} received {:?} from {:?}",
                        receiver_id, msg, origin_id
                    );

                    assert_eq!(origin_id, PeerId::from(i));
                    assert_eq!(msg, message)
                }
            }
        }

        Ok(())
    }

    #[tokio::test]
    async fn test_iroh_api_connections() -> anyhow::Result<()> {
        let secret_key = SecretKey::generate();
        let public_key = secret_key.public();

        tokio::spawn(async { echo(secret_key).await });

        let connections = IrohApiConnections::new(
            [(PeerId::from(0), public_key)].into_iter().collect(),
            TaskGroup::new(),
        )
        .await
        .expect("Failed to build connections");

        for i in 0_u64.. {
            let request = i.to_be_bytes().to_vec();

            println!("request echo for: {:?}", request);

            if let Some(response) = connections.request(PeerId::from(0), request.clone()).await {
                assert_eq!(request, response);
                break;
            }

            sleep(Duration::from_millis(100)).await;
        }

        Ok(())
    }

    async fn echo(secret_key: SecretKey) {
        let endpoint = Endpoint::builder()
            .secret_key(secret_key)
            .alpns(vec![FEDIMINT_P2P_ALPN.to_vec()])
            .bind()
            .await
            .expect("Failed to bind to echo endpoint");

        loop {
            let connection = endpoint
                .accept()
                .await
                .expect("Failed to accept incoming connections")
                .await
                .expect("Fail to connect");

            println!("Accepted Connection");

            while let Ok((mut send_stream, mut receive_stream)) = connection.accept_bi().await {
                println!("Handling Request");

                let echo = receive_stream
                    .read_to_end(100_000)
                    .await
                    .expect("Failed to read echo from stream");

                println!("responding with echo for: {:?}", echo);

                send_stream
                    .write_all(&echo)
                    .await
                    .expect("Failed to write echo to stream");

                send_stream.finish().expect("Failed to finish send stream");
            }
        }
    }
}

/// Connection manager that automatically reconnects to peers
#[derive(Clone)]
pub struct IrohApiConnections {
    connections: HashMap<PeerId, IrohApiConnection>,
}

impl IrohApiConnections {
    /// Creates a new `ReconnectPeerConnections` connection manager from a
    /// network config and a [`Connector`](crate::net::connect::Connector).
    /// See [`ReconnectPeerConnections`] for requirements on the
    /// `Connector`.
    #[instrument(skip_all)]
    pub(crate) async fn new(
        peers: BTreeMap<PeerId, NodeId>,
        task_group: TaskGroup,
    ) -> anyhow::Result<Self> {
        let endpoint = Endpoint::builder()
            .alpns(vec![FEDIMINT_P2P_ALPN.to_vec()])
            .bind()
            .await?;

        let connections = peers
            .iter()
            .map(|(peer_id, peer_node_id)| {
                let connection =
                    IrohApiConnection::new(endpoint.clone(), *peer_id, *peer_node_id, &task_group);

                (*peer_id, connection)
            })
            .collect();

        Ok(IrohApiConnections { connections })
    }

    async fn request(&self, peer_id: PeerId, message: Vec<u8>) -> Option<Vec<u8>> {
        self.connections
            .get(&peer_id)
            .expect("Cannot find peer api connection for peer id")
            .request(message)
            .await
    }
}

#[derive(Clone)]
struct IrohApiConnection {
    request_sender: async_channel::Sender<(Vec<u8>, oneshot::Sender<Vec<u8>>)>,
}

impl IrohApiConnection {
    #[allow(clippy::too_many_arguments)]
    fn new(
        endpoint: Endpoint,
        peer_id: PeerId,
        peer_node_id: NodeId,
        task_group: &TaskGroup,
    ) -> IrohApiConnection {
        let (request_sender, request_receiver) = async_channel::bounded(1024);

        let tg = task_group.clone();

        task_group.spawn(
            format!("io-thread-peer-{peer_id}"),
            move |handle| async move {
                Self::run_connection_state_machine(
                    endpoint,
                    request_receiver,
                    peer_id,
                    peer_node_id,
                    tg,
                    &handle,
                )
                .await;
            },
        );

        IrohApiConnection { request_sender }
    }

    async fn request(&self, request: Vec<u8>) -> Option<Vec<u8>> {
        let (response_sender, response_receiver) = oneshot::channel();

        if self
            .request_sender
            .send((request, response_sender))
            .await
            .is_err()
        {
            warn!(target: LOG_NET_PEER, "Could not send outgoing message");
        }

        response_receiver.await.ok()
    }

    async fn run_connection_state_machine(
        endpoint: Endpoint,
        request_receiver: async_channel::Receiver<(Vec<u8>, oneshot::Sender<Vec<u8>>)>,
        peer_id: PeerId,
        peer_node_id: NodeId,
        task_group: TaskGroup,
        task_handle: &TaskHandle,
    ) {
        info!(target: LOG_NET_PEER, "Starting connection state machine for peer {peer_id}");

        let mut state_machine = IrohApiConnectionSM {
            common: IrohApiConnectionSMCommon {
                endpoint,
                request_receiver,
                peer_node_id,
                peer_id,
                task_group,
            },
            state: IrohApiConnectionSMState::Disconnected(Instant::now()),
        };

        loop {
            tokio::select! {
                new_state =  state_machine.state_transition() => {
                    if let Some(new_state) = new_state {
                        state_machine = new_state;
                    } else {
                        break;
                    }
                },
                () = task_handle.make_shutdown_rx() => {
                    break;
                },
            }
        }

        info!(target: LOG_NET_PEER, "Shutting down connection state machine for peer {peer_id}");
    }
}

struct IrohApiConnectionSM {
    common: IrohApiConnectionSMCommon,
    state: IrohApiConnectionSMState,
}

struct IrohApiConnectionSMCommon {
    endpoint: Endpoint,
    request_receiver: async_channel::Receiver<(Vec<u8>, oneshot::Sender<Vec<u8>>)>,
    peer_id: PeerId,
    peer_node_id: NodeId,
    task_group: TaskGroup,
}

enum IrohApiConnectionSMState {
    Disconnected(Instant),
    Connected(Connection),
}

impl IrohApiConnectionSM {
    async fn state_transition(mut self) -> Option<Self> {
        match self.state {
            IrohApiConnectionSMState::Disconnected(disconnected) => {
                let state = self
                    .common
                    .state_transition_disconnected(disconnected)
                    .await?;

                Some(IrohApiConnectionSM {
                    common: self.common,
                    state,
                })
            }
            IrohApiConnectionSMState::Connected(connected) => {
                let state = self.common.state_transition_connected(connected).await?;

                Some(IrohApiConnectionSM {
                    common: self.common,
                    state,
                })
            }
        }
    }
}

impl IrohApiConnectionSMCommon {
    async fn state_transition_connected(
        &mut self,
        connection: Connection,
    ) -> Option<IrohApiConnectionSMState> {
        tokio::select! {
            message = self.request_receiver.recv() => {
                match self.handle_request(&connection, message.ok()?).await {
                    Ok(()) => Some(IrohApiConnectionSMState::Connected(connection)),
                    Err(e) => Some(self.disconnected(e)),
                }
            },
            error = connection.closed() => {
                Some(self.disconnected(error.into()))
            }
        }
    }

    fn connected(&mut self, connection: Connection) -> IrohApiConnectionSMState {
        info!("Peer {} is connected", self.peer_id);

        IrohApiConnectionSMState::Connected(connection)
    }

    fn disconnected(&self, error: anyhow::Error) -> IrohApiConnectionSMState {
        info!(target: LOG_NET_PEER, "Peer {} is disconnected: {}", self.peer_id, error);

        IrohApiConnectionSMState::Disconnected(Instant::now())
    }

    async fn handle_request(
        &self,
        connection: &Connection,
        request: (Vec<u8>, oneshot::Sender<Vec<u8>>),
    ) -> anyhow::Result<()> {
        let (mut send_stream, mut receive_stream) = connection.open_bi().await?;

        send_stream.write_all(&request.0).await?;

        send_stream.finish()?;

        self.task_group.spawn("request task", |handle| async move {
            tokio::select! {
                response = receive_stream.read_to_end(100_000) => {
                    if let Ok(response) = response {
                        request.1.send(response).ok();
                    }
                },
                () = handle.make_shutdown_rx() => {},
            }
        });

        Ok(())
    }

    async fn state_transition_disconnected(
        &mut self,
        reconnect_at: Instant,
    ) -> Option<IrohApiConnectionSMState> {
        tokio::select! {
            _ = self.request_receiver.recv() => {
                Some(IrohApiConnectionSMState::Disconnected(reconnect_at))
            },
            () = tokio::time::sleep_until(reconnect_at) => {
                Some(self.reconnect().await)
            },
        }
    }

    async fn reconnect(&mut self) -> IrohApiConnectionSMState {
        if let Some(remote_info) = self.endpoint.remote_info(self.peer_node_id) {
            let addr = NodeAddr::from_parts(
                self.peer_node_id,
                remote_info.relay_url.map(|relay_info| relay_info.relay_url),
                remote_info
                    .addrs
                    .into_iter()
                    .map(|addr_info| addr_info.addr)
                    .collect(),
            );

            if let Ok(connection) = self.endpoint.connect(addr, FEDIMINT_P2P_ALPN).await {
                return self.connected(connection);
            };
        }

        if let Some(connection) = connect(&self.endpoint, self.peer_node_id).await {
            return self.connected(connection);
        };

        IrohApiConnectionSMState::Disconnected(Instant::now() + Duration::from_secs(10))
    }
}
