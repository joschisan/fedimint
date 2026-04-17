use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use futures::future::select_all;
use peer_handle::PeerHandle;
use picomint_api_client::config::ConsensusConfig;
pub use picomint_core::config::{FederationId, PeerEndpoint};
use picomint_core::envs::is_running_in_test_env;
use picomint_core::invite_code::InviteCode;
use picomint_core::module::{ApiAuth, CORE_CONSENSUS_VERSION};
use picomint_core::setup_code::PeerSetupCode;
use picomint_core::task::sleep;
use picomint_core::{NumPeersExt, PeerId, secp256k1, timing};
use picomint_ln_common::config::LightningConfigPrivate;
use picomint_logging::LOG_NET_PEER_DKG;
use picomint_mint_common::config::{MintConfig, MintConfigPrivate};
use picomint_wallet_common::config::{WalletConfig, WalletConfigPrivate};
use rand::rngs::OsRng;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::select;
use tracing::{error, info, warn};

use crate::p2p::{P2PMessage, P2PStatusReceivers, Recipient, ReconnectP2PConnections};
use picomint_encoding::{Decodable, Encodable};

pub mod db;
pub mod dkg;
pub mod dkg_g1;
pub mod dkg_g2;
pub mod peer_handle;
pub mod setup;

/// How many concurrent Iroh API connections the server will accept.
pub const MAX_CLIENT_CONNECTIONS: u32 = 1000;

/// AlephBFT round delay (ms). Byzantine-fault-only; the ordering floor is
/// dominated by network latency in practice.
pub const BROADCAST_ROUND_DELAY_MS: u16 = 50;

/// AlephBFT rounds per session. Controls session duration (3 min prod / 10 s
/// test).
const DEFAULT_BROADCAST_ROUNDS_PER_SESSION: u16 = 3600;
const TEST_BROADCAST_ROUNDS_PER_SESSION: u16 = 200;

fn broadcast_rounds_per_session() -> u16 {
    if is_running_in_test_env() {
        TEST_BROADCAST_ROUNDS_PER_SESSION
    } else {
        DEFAULT_BROADCAST_ROUNDS_PER_SESSION
    }
}

#[allow(clippy::unsafe_derive_deserialize)] // clippy fires on `select!` https://github.com/rust-lang/rust-clippy/issues/13062
#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
/// Full picomint server config (persisted in redb).
pub struct ServerConfig {
    /// Federation-wide config, identical across peers
    pub consensus: ConsensusConfig,
    /// Per-peer secrets (identity + DKG keys)
    pub private: ServerConfigPrivate,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct ServerConfigPrivate {
    /// Our peer id
    pub identity: PeerId,
    /// Secret key for our single iroh endpoint (p2p + api)
    pub iroh_sk: iroh::SecretKey,
    /// Secret key for the atomic broadcast to sign messages
    pub broadcast_secret_key: SecretKey,
    /// Private key material for the mint module
    pub mint: MintConfigPrivate,
    /// Private key material for the lightning module
    pub ln: LightningConfigPrivate,
    /// Private key material for the wallet module
    pub wallet: WalletConfigPrivate,
}

/// All the info we configure prior to config gen starting
#[derive(Debug, Clone)]
pub struct ConfigGenSettings {
    /// Bind address for our P2P connection
    pub p2p_addr: SocketAddr,
    /// Web UI bind address + admin password. `None` disables the UI and
    /// requires all admin actions (including DKG setup) to go through the
    /// CLI. `main` rejects boot if `UI_ADDR` is set without `UI_PASSWORD`
    /// or vice versa, so these are always populated together.
    pub ui_config: Option<(SocketAddr, ApiAuth)>,
    /// Bitcoin network for the federation
    pub network: bitcoin::Network,
}

#[derive(Debug, Clone)]
/// All the parameters necessary for generating the `ServerConfig` during setup
///
/// * Guardians can create the parameters using a setup UI or CLI tool
/// * Used for distributed or trusted config generation
pub struct ConfigGenParams {
    /// Our own peer id
    pub identity: PeerId,
    /// Secret key for our single iroh endpoint (p2p + api)
    pub iroh_sk: iroh::SecretKey,
    /// Endpoints of all servers
    pub peers: BTreeMap<PeerId, PeerSetupCode>,
    /// Guardian-defined key-value pairs that will be passed to the client
    pub meta: BTreeMap<String, String>,
    /// Bitcoin network for this federation
    pub network: bitcoin::Network,
}

impl ServerConfig {
    /// Assemble a fresh `ServerConfig` from config-gen parameters, the
    /// threshold-signing key pair we generated locally, and the per-module
    /// DKG outputs.
    pub fn from(
        params: ConfigGenParams,
        identity: PeerId,
        broadcast_public_keys: BTreeMap<PeerId, PublicKey>,
        broadcast_secret_key: SecretKey,
        mint: MintConfig,
        ln: picomint_ln_common::config::LightningConfig,
        wallet: WalletConfig,
    ) -> Self {
        let consensus = ConsensusConfig {
            version: CORE_CONSENSUS_VERSION,
            broadcast_public_keys,
            broadcast_rounds_per_session: broadcast_rounds_per_session(),
            iroh_endpoints: params.iroh_endpoints(),
            mint: mint.consensus,
            ln: ln.consensus,
            wallet: wallet.consensus,
            meta: params.meta.clone(),
        };

        let private = ServerConfigPrivate {
            identity,
            iroh_sk: params.iroh_sk,
            broadcast_secret_key,
            mint: mint.private,
            ln: ln.private,
            wallet: wallet.private,
        };

        Self { consensus, private }
    }

    pub fn get_invite_code(&self) -> InviteCode {
        InviteCode::new(
            self.private.iroh_sk.public(),
            self.private.identity,
            self.consensus.calculate_federation_id(),
        )
    }

    /// Bundle the current peer's typed configs back into per-module
    /// `*Config` values for passing into the module constructors.
    pub fn mint_config(&self) -> MintConfig {
        MintConfig {
            private: self.private.mint.clone(),
            consensus: self.consensus.mint.clone(),
        }
    }

    pub fn ln_config(&self) -> picomint_ln_common::config::LightningConfig {
        picomint_ln_common::config::LightningConfig {
            private: self.private.ln.clone(),
            consensus: self.consensus.ln.clone(),
        }
    }

    pub fn wallet_config(&self) -> WalletConfig {
        WalletConfig {
            private: self.private.wallet.clone(),
            consensus: self.consensus.wallet.clone(),
        }
    }

    pub fn validate_config(&self, identity: &PeerId) -> anyhow::Result<()> {
        let endpoints = &self.consensus.iroh_endpoints;
        let my_public_key = self
            .private
            .broadcast_secret_key
            .public_key(&Secp256k1::new());

        if Some(&my_public_key) != self.consensus.broadcast_public_keys.get(identity) {
            bail!("Broadcast secret key doesn't match corresponding public key");
        }
        if endpoints.keys().max().copied().map(PeerId::to_usize) != Some(endpoints.len() - 1) {
            bail!("Peer ids are not indexed from 0");
        }
        if endpoints.keys().min().copied() != Some(PeerId::from(0)) {
            bail!("Peer ids are not indexed from 0");
        }

        picomint_mint_server::validate_config(identity, &self.mint_config())?;
        picomint_ln_server::validate_config(identity, &self.ln_config())?;
        picomint_wallet_server::validate_config(identity, &self.wallet_config())?;

        Ok(())
    }

    /// Runs the distributed key gen algorithm
    pub async fn distributed_gen(
        params: &ConfigGenParams,
        connections: ReconnectP2PConnections<P2PMessage>,
        mut p2p_status_receivers: P2PStatusReceivers,
    ) -> anyhow::Result<Self> {
        let _timing /* logs on drop */ = timing::TimeReporter::new("distributed-gen").info();

        info!(
            target: LOG_NET_PEER_DKG,
            "Waiting for all p2p connections to open..."
        );

        loop {
            let mut pending_connection_receivers: Vec<_> = p2p_status_receivers
                .iter_mut()
                .filter_map(|(p, r)| {
                    r.mark_unchanged();
                    r.borrow().is_none().then_some((*p, r.clone()))
                })
                .collect();

            if pending_connection_receivers.is_empty() {
                break;
            }

            let disconnected_peers = pending_connection_receivers
                .iter()
                .map(|entry| entry.0)
                .collect::<Vec<PeerId>>();

            info!(
                target: LOG_NET_PEER_DKG,
                pending = ?disconnected_peers,
                "Waiting for all p2p connections to open..."
            );

            select! {
                _ = select_all(pending_connection_receivers.iter_mut().map(|r| Box::pin(r.1.changed()))) => {}
                () = sleep(Duration::from_secs(10)) => {}
            }
        }

        let checksum = params.peers.consensus_hash_sha256();

        info!(
            target: LOG_NET_PEER_DKG,
            "Comparing connection codes checksum {checksum}..."
        );

        connections.send(Recipient::Everyone, P2PMessage::Checksum(checksum));

        for peer in params
            .peer_ids()
            .into_iter()
            .filter(|p| *p != params.identity)
        {
            let peer_message = connections
                .receive_from_peer(peer)
                .await
                .context("Unexpected shutdown of p2p connections")?;

            if peer_message != P2PMessage::Checksum(checksum) {
                error!(
                    target: LOG_NET_PEER_DKG,
                    expected = ?P2PMessage::Checksum(checksum),
                    received = ?peer_message,
                    "Peer {peer} has sent invalid connection code checksum message"
                );

                bail!("Peer {peer} has sent invalid connection code checksum message");
            }

            info!(
                target: LOG_NET_PEER_DKG,
                "Peer {peer} has sent valid connection code checksum message"
            );
        }

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation..."
        );

        let handle = PeerHandle::new(
            params.peer_ids().to_num_peers(),
            params.identity,
            &connections,
        );

        let (broadcast_sk, broadcast_pk) = secp256k1::generate_keypair(&mut OsRng);

        use picomint_server_core::config::PeerHandleOpsExt as _;
        let broadcast_public_keys = handle.exchange_encodable(broadcast_pk).await?;

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation for module of kind mint..."
        );
        let mint = picomint_mint_server::distributed_gen(&handle).await?;

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation for module of kind ln..."
        );
        let ln = picomint_ln_server::distributed_gen(&handle, params.network).await?;

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation for module of kind wallet..."
        );
        let wallet = picomint_wallet_server::distributed_gen(&handle, params.network).await?;

        let cfg = ServerConfig::from(
            params.clone(),
            params.identity,
            broadcast_public_keys,
            broadcast_sk,
            mint,
            ln,
            wallet,
        );

        let checksum = cfg.consensus.consensus_hash_sha256();

        info!(
            target: LOG_NET_PEER_DKG,
            "Comparing consensus config checksum {checksum}..."
        );

        connections.send(Recipient::Everyone, P2PMessage::Checksum(checksum));

        for peer in params
            .peer_ids()
            .into_iter()
            .filter(|p| *p != params.identity)
        {
            let peer_message = connections
                .receive_from_peer(peer)
                .await
                .context("Unexpected shutdown of p2p connections")?;

            if peer_message != P2PMessage::Checksum(checksum) {
                warn!(
                    target: LOG_NET_PEER_DKG,
                    expected = ?P2PMessage::Checksum(checksum),
                    received = ?peer_message,
                    config = ?cfg.consensus,
                    "Peer {peer} has sent invalid consensus config checksum message"
                );

                bail!("Peer {peer} has sent invalid consensus config checksum message");
            }

            info!(
                target: LOG_NET_PEER_DKG,
                "Peer {peer} has sent valid consensus config checksum message"
            );
        }

        info!(
            target: LOG_NET_PEER_DKG,
            "Config generation has completed successfully!"
        );

        Ok(cfg)
    }
}

impl ConfigGenParams {
    pub fn peer_ids(&self) -> Vec<PeerId> {
        self.peers.keys().copied().collect()
    }

    pub fn iroh_endpoints(&self) -> BTreeMap<PeerId, PeerEndpoint> {
        self.peers
            .iter()
            .map(|(id, peer)| {
                let endpoint = PeerEndpoint {
                    name: peer.name.clone(),
                    node_id: peer.pk,
                };
                (*id, endpoint)
            })
            .collect()
    }
}
