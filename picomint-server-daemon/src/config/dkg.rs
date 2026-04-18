use std::collections::BTreeMap;

use anyhow::Context;
use bls12_381::{G1Projective, G2Projective, Scalar};
use picomint_core::{NumPeers, PeerId};
use picomint_encoding::{Decodable, Encodable};
use picomint_logging::LOG_NET_PEER_DKG;
use tracing::info;

use super::dkg_g1::run_dkg_g1;
use super::dkg_g2::run_dkg_g2;
use crate::p2p::{P2PMessage, Recipient, ReconnectP2PConnections};

/// A handle passed to DKG routines. Encapsulates the peer-id + p2p connection
/// machinery each module needs to run distributed key generation or exchange
/// arbitrary data with the other guardians.
#[non_exhaustive]
pub struct DkgHandle<'a> {
    #[doc(hidden)]
    pub num_peers: NumPeers,
    #[doc(hidden)]
    pub identity: PeerId,
    #[doc(hidden)]
    pub connections: &'a ReconnectP2PConnections<P2PMessage>,
}

impl<'a> DkgHandle<'a> {
    pub fn new(
        num_peers: NumPeers,
        identity: PeerId,
        connections: &'a ReconnectP2PConnections<P2PMessage>,
    ) -> Self {
        Self {
            num_peers,
            identity,
            connections,
        }
    }

    pub fn num_peers(&self) -> NumPeers {
        self.num_peers
    }

    pub async fn run_dkg_g1(&self) -> anyhow::Result<(Vec<G1Projective>, Scalar)> {
        info!(
            target: LOG_NET_PEER_DKG,
            "Running distributed key generation for group G1..."
        );

        run_dkg_g1(self.num_peers, self.identity, self.connections).await
    }

    pub async fn run_dkg_g2(&self) -> anyhow::Result<(Vec<G2Projective>, Scalar)> {
        info!(
            target: LOG_NET_PEER_DKG,
            "Running distributed key generation for group G2..."
        );

        run_dkg_g2(self.num_peers, self.identity, self.connections).await
    }

    /// Exchange a `DkgPeerMsg::Module(Vec<u8>)` with all peers. All peers must
    /// be online and submit a response. The caller's message is included in
    /// the returned map under its own `PeerId`.
    pub async fn exchange_bytes(
        &self,
        bytes: Vec<u8>,
    ) -> anyhow::Result<BTreeMap<PeerId, Vec<u8>>> {
        info!(
            target: LOG_NET_PEER_DKG,
            "Exchanging raw bytes..."
        );

        let mut peer_data: BTreeMap<PeerId, Vec<u8>> = BTreeMap::new();

        self.connections
            .send(Recipient::Everyone, P2PMessage::Encodable(bytes.clone()));

        peer_data.insert(self.identity, bytes);

        for peer in self.num_peers.peer_ids().filter(|p| *p != self.identity) {
            let message = self
                .connections
                .receive_from_peer(peer)
                .await
                .context("Unexpected shutdown of p2p connections")?;

            match message {
                P2PMessage::Encodable(bytes) => {
                    peer_data.insert(peer, bytes);
                }
                message => {
                    anyhow::bail!("Invalid message from {peer}: {message:?}");
                }
            }
        }

        Ok(peer_data)
    }

    pub async fn exchange_encodable<T: Encodable + Decodable + Send + Sync>(
        &self,
        data: T,
    ) -> anyhow::Result<BTreeMap<PeerId, T>> {
        let mut decoded = BTreeMap::new();
        for (k, bytes) in self.exchange_bytes(data.consensus_encode_to_vec()).await? {
            decoded.insert(k, T::consensus_decode_exact(&bytes)?);
        }
        Ok(decoded)
    }
}
