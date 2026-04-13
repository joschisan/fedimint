use std::collections::BTreeMap;
use std::net::SocketAddr;

use anyhow::{Context as _, ensure};
use async_trait::async_trait;
use fedimint_core::PeerId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::net::STANDARD_FEDIMINT_P2P_PORT;
use fedimint_core::net::iroh::build_iroh_endpoint;
use fedimint_core::util::SafeUrl;
use fedimint_logging::LOG_NET_IROH;
use fedimint_server_core::dashboard_ui::ConnectionType;
use iroh::{Endpoint, PublicKey, SecretKey};
use tracing::trace;

use super::IP2PConnector;
use crate::net::p2p_connection::{DynP2PConnection, IP2PConnection as _};

/// Parses the host and port from a url
pub fn parse_p2p(url: &SafeUrl) -> anyhow::Result<String> {
    ensure!(url.scheme() == "fedimint", "p2p url has invalid scheme");

    let host = url.host_str().context("p2p url is missing host")?;

    let port = url.port().unwrap_or(STANDARD_FEDIMINT_P2P_PORT);

    Ok(format!("{host}:{port}"))
}

#[derive(Debug, Clone)]
pub struct IrohConnector {
    /// Map of all peers' connection information we want to be connected to
    pub(crate) node_ids: BTreeMap<PeerId, PublicKey>,
    /// The Iroh endpoint
    pub(crate) endpoint: Endpoint,
}

pub(crate) const FEDIMINT_P2P_ALPN: &[u8] = b"FEDIMINT_P2P_ALPN";

impl IrohConnector {
    pub async fn new(
        secret_key: SecretKey,
        p2p_bind_addr: SocketAddr,
        node_ids: BTreeMap<PeerId, PublicKey>,
    ) -> anyhow::Result<Self> {
        let identity = *node_ids
            .iter()
            .find(|entry| entry.1 == &secret_key.public())
            .expect("Our public key is not part of the keyset")
            .0;

        let endpoint = build_iroh_endpoint(secret_key, p2p_bind_addr, FEDIMINT_P2P_ALPN).await?;

        Ok(Self {
            node_ids: node_ids
                .into_iter()
                .filter(|entry| entry.0 != identity)
                .collect(),
            endpoint,
        })
    }
}

#[async_trait]
impl<M> IP2PConnector<M> for IrohConnector
where
    M: Encodable + Decodable + Send + 'static,
{
    fn peers(&self) -> Vec<PeerId> {
        self.node_ids.keys().copied().collect()
    }

    async fn connect(&self, peer: PeerId) -> anyhow::Result<DynP2PConnection<M>> {
        let node_id = *self.node_ids.get(&peer).expect("No node id found for peer");

        let connection = self.endpoint.connect(node_id, FEDIMINT_P2P_ALPN).await?;

        Ok(connection.into_dyn())
    }

    async fn accept(&self) -> anyhow::Result<(PeerId, DynP2PConnection<M>)> {
        let connection = self
            .endpoint
            .accept()
            .await
            .context("Listener closed unexpectedly")?
            .accept()?
            .await?;

        let node_id = connection.remote_id();

        let auth_peer = self
            .node_ids
            .iter()
            .find(|entry| entry.1 == &node_id)
            .with_context(|| format!("Node id {node_id} is unknown"))?
            .0;

        Ok((*auth_peer, connection.into_dyn()))
    }

    fn connection_type(&self, _peer: PeerId) -> Option<ConnectionType> {
        // Connection type monitoring was removed in iroh 0.97
        None
    }
}
