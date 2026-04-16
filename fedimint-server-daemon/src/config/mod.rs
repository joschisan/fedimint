use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::{Context, bail};
use fedimint_api_client::wire::{LN_INSTANCE_ID, MINT_INSTANCE_ID, WALLET_INSTANCE_ID};
pub use fedimint_core::config::{
    ClientConfig, ClientModuleConfig, FederationId, GlobalClientConfig, PeerEndpoint,
};
use fedimint_core::core::ModuleKind;
use fedimint_core::envs::is_running_in_test_env;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::{CORE_CONSENSUS_VERSION, CoreConsensusVersion};
use fedimint_core::net::peers::{DynP2PConnections, Recipient};
use fedimint_core::setup_code::PeerSetupCode;
use fedimint_core::task::sleep;
use fedimint_core::util::SafeUrl;
use fedimint_core::{NumPeersExt, PeerId, secp256k1, timing};
use fedimint_lnv2_common::config::{LightningConfigConsensus, LightningConfigPrivate};
use fedimint_logging::LOG_NET_PEER_DKG;
use fedimint_mintv2_common::config::{MintConfig, MintConfigConsensus, MintConfigPrivate};
use fedimint_walletv2_common::config::{WalletConfig, WalletConfigConsensus, WalletConfigPrivate};
use futures::future::select_all;
use peer_handle::PeerHandle;
use rand::rngs::OsRng;
use secp256k1::{PublicKey, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use tokio::select;
use tracing::{error, info, warn};

use crate::fedimint_core::encoding::Encodable;
use crate::net::p2p::P2PStatusReceivers;
use crate::p2p::P2PMessage;

pub mod dkg;
pub mod dkg_g1;
pub mod dkg_g2;
pub mod io;
pub mod peer_handle;
pub mod setup;

/// The default maximum open connections the API can handle
pub const DEFAULT_MAX_CLIENT_CONNECTIONS: u32 = 1000;

/// Consensus broadcast settings that result in 3 minutes session time
const DEFAULT_BROADCAST_ROUND_DELAY_MS: u16 = 50;
const DEFAULT_BROADCAST_ROUNDS_PER_SESSION: u16 = 3600;

fn default_broadcast_rounds_per_session() -> u16 {
    DEFAULT_BROADCAST_ROUNDS_PER_SESSION
}

/// Consensus broadcast settings that result in 10 seconds session time
const DEFAULT_TEST_BROADCAST_ROUND_DELAY_MS: u16 = 50;
const DEFAULT_TEST_BROADCAST_ROUNDS_PER_SESSION: u16 = 200;

#[allow(clippy::unsafe_derive_deserialize)] // clippy fires on `select!` https://github.com/rust-lang/rust-clippy/issues/13062
#[derive(Debug, Clone, Serialize, Deserialize)]
/// All the serializable configuration for the fedimint server
pub struct ServerConfig {
    /// Contains all configuration that needs to be the same for every server
    pub consensus: ServerConfigConsensus,
    /// Contains all configuration that is locally configurable and not secret
    pub local: ServerConfigLocal,
    /// Contains all configuration that will be encrypted such as private key
    /// material
    pub private: ServerConfigPrivate,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigPrivate {
    /// Secret key for our iroh api endpoint
    pub iroh_api_sk: iroh::SecretKey,
    /// Secret key for our iroh p2p endpoint
    pub iroh_p2p_sk: iroh::SecretKey,
    /// Secret key for the atomic broadcast to sign messages
    pub broadcast_secret_key: SecretKey,
    /// Private key material for the mint module
    pub mint: MintConfigPrivate,
    /// Private key material for the lightning module
    pub ln: LightningConfigPrivate,
    /// Private key material for the wallet module
    pub wallet: WalletConfigPrivate,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encodable)]
pub struct ServerConfigConsensus {
    /// The version of the binary code running
    pub code_version: String,
    /// Agreed on core consensus version
    pub version: CoreConsensusVersion,
    /// Public keys for the atomic broadcast to authenticate messages
    pub broadcast_public_keys: BTreeMap<PeerId, PublicKey>,
    /// Number of rounds per session.
    #[serde(default = "default_broadcast_rounds_per_session")]
    pub broadcast_rounds_per_session: u16,
    /// Public keys for all iroh api and p2p endpoints
    pub iroh_endpoints: BTreeMap<PeerId, PeerIrohEndpoints>,
    /// Additional config the federation wants to transmit to the clients
    pub meta: BTreeMap<String, String>,
    /// Consensus config for the mint module
    pub mint: MintConfigConsensus,
    /// Consensus config for the lightning module
    pub ln: LightningConfigConsensus,
    /// Consensus config for the wallet module
    pub wallet: WalletConfigConsensus,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encodable)]
pub struct PeerIrohEndpoints {
    /// The peer's name
    pub name: String,
    /// Public key for our iroh api endpoint
    pub api_pk: iroh::PublicKey,
    /// Public key for our iroh p2p endpoint
    pub p2p_pk: iroh::PublicKey,
}

// FIXME: (@leonardo) Should this have another field for the expected transport
// ? (e.g. clearnet/tor/...)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfigLocal {
    /// Our peer id (generally should not change)
    pub identity: PeerId,
    /// How many API connections we will accept
    pub max_connections: u32,
    /// Influences the atomic broadcast ordering latency, should be higher than
    /// the expected latency between peers so everyone can get proposed
    /// consensus items confirmed. This is only relevant for byzantine
    /// faults.
    pub broadcast_round_delay_ms: u16,
}

/// All the info we configure prior to config gen starting
#[derive(Debug, Clone)]
pub struct ConfigGenSettings {
    /// Bind address for our P2P connection
    pub p2p_bind: SocketAddr,
    /// Bind address for our UI connection (always http)
    pub ui_bind: SocketAddr,
    /// Optional URL of the Iroh DNS server
    pub iroh_dns: Option<SafeUrl>,
    /// Optional URLs of the Iroh relays to register on
    pub iroh_relays: Vec<SafeUrl>,
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
    /// Secret key for our iroh api endpoint
    pub iroh_api_sk: iroh::SecretKey,
    /// Secret key for our iroh p2p endpoint
    pub iroh_p2p_sk: iroh::SecretKey,
    /// Endpoints of all servers
    pub peers: BTreeMap<PeerId, PeerSetupCode>,
    /// Guardian-defined key-value pairs that will be passed to the client
    pub meta: BTreeMap<String, String>,
    /// Bitcoin network for this federation
    pub network: bitcoin::Network,
}

impl ServerConfigConsensus {
    pub fn api_endpoints(&self) -> BTreeMap<PeerId, PeerEndpoint> {
        self.iroh_endpoints
            .iter()
            .map(|(peer, endpoints)| {
                let endpoint = PeerEndpoint {
                    name: endpoints.name.clone(),
                    node_id: endpoints.api_pk,
                };

                (*peer, endpoint)
            })
            .collect()
    }

    pub fn to_client_config(&self) -> ClientConfig {
        let modules = BTreeMap::from([
            (
                MINT_INSTANCE_ID,
                ClientModuleConfig::from_typed(
                    MINT_INSTANCE_ID,
                    ModuleKind::from_static_str("mintv2"),
                    fedimint_mintv2_common::MODULE_CONSENSUS_VERSION,
                    self.mint.to_client(),
                )
                .expect("Encoding mint client config must succeed"),
            ),
            (
                LN_INSTANCE_ID,
                ClientModuleConfig::from_typed(
                    LN_INSTANCE_ID,
                    ModuleKind::from_static_str("lnv2"),
                    fedimint_lnv2_common::MODULE_CONSENSUS_VERSION,
                    self.ln.to_client(),
                )
                .expect("Encoding ln client config must succeed"),
            ),
            (
                WALLET_INSTANCE_ID,
                ClientModuleConfig::from_typed(
                    WALLET_INSTANCE_ID,
                    ModuleKind::from_static_str("walletv2"),
                    fedimint_walletv2_common::MODULE_CONSENSUS_VERSION,
                    self.wallet.to_client(),
                )
                .expect("Encoding wallet client config must succeed"),
            ),
        ]);

        ClientConfig {
            global: GlobalClientConfig {
                api_endpoints: self.api_endpoints(),
                broadcast_public_keys: Some(self.broadcast_public_keys.clone()),
                consensus_version: self.version,
                meta: self.meta.clone(),
            },
            modules,
        }
    }
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
        ln: fedimint_lnv2_common::config::LightningConfig,
        wallet: WalletConfig,
        code_version: String,
    ) -> Self {
        let consensus = ServerConfigConsensus {
            code_version,
            version: CORE_CONSENSUS_VERSION,
            broadcast_public_keys,
            broadcast_rounds_per_session: if is_running_in_test_env() {
                DEFAULT_TEST_BROADCAST_ROUNDS_PER_SESSION
            } else {
                DEFAULT_BROADCAST_ROUNDS_PER_SESSION
            },
            iroh_endpoints: params.iroh_endpoints(),
            mint: mint.consensus,
            ln: ln.consensus,
            wallet: wallet.consensus,
            meta: params.meta.clone(),
        };

        let local = ServerConfigLocal {
            identity,
            max_connections: DEFAULT_MAX_CLIENT_CONNECTIONS,
            broadcast_round_delay_ms: if is_running_in_test_env() {
                DEFAULT_TEST_BROADCAST_ROUND_DELAY_MS
            } else {
                DEFAULT_BROADCAST_ROUND_DELAY_MS
            },
        };

        let private = ServerConfigPrivate {
            iroh_api_sk: params.iroh_api_sk,
            iroh_p2p_sk: params.iroh_p2p_sk,
            broadcast_secret_key,
            mint: mint.private,
            ln: ln.private,
            wallet: wallet.private,
        };

        Self {
            consensus,
            local,
            private,
        }
    }

    pub fn get_invite_code(&self) -> InviteCode {
        InviteCode::new(
            self.consensus.api_endpoints()[&self.local.identity].node_id,
            self.local.identity,
            self.calculate_federation_id(),
        )
    }

    pub fn calculate_federation_id(&self) -> FederationId {
        FederationId(self.consensus.api_endpoints().consensus_hash())
    }

    /// Bundle the current peer's typed configs back into per-module
    /// `*Config` values for passing into the module constructors.
    pub fn mint_config(&self) -> MintConfig {
        MintConfig {
            private: self.private.mint.clone(),
            consensus: self.consensus.mint.clone(),
        }
    }

    pub fn ln_config(&self) -> fedimint_lnv2_common::config::LightningConfig {
        fedimint_lnv2_common::config::LightningConfig {
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
        let endpoints = self.consensus.api_endpoints().clone();
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

        fedimint_mintv2_server::validate_config(identity, &self.mint_config())?;
        fedimint_lnv2_server::validate_config(identity, &self.ln_config())?;
        fedimint_walletv2_server::validate_config(identity, &self.wallet_config())?;

        Ok(())
    }

    /// Runs the distributed key gen algorithm
    pub async fn distributed_gen(
        params: &ConfigGenParams,
        code_version_str: String,
        connections: DynP2PConnections<P2PMessage>,
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

        use fedimint_server_core::config::PeerHandleOpsExt as _;
        let broadcast_public_keys = handle.exchange_encodable(broadcast_pk).await?;

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation for module of kind mintv2..."
        );
        let mint = fedimint_mintv2_server::distributed_gen(&handle).await?;

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation for module of kind lnv2..."
        );
        let ln = fedimint_lnv2_server::distributed_gen(&handle, params.network).await?;

        info!(
            target: LOG_NET_PEER_DKG,
            "Running config generation for module of kind walletv2..."
        );
        let wallet = fedimint_walletv2_server::distributed_gen(&handle, params.network).await?;

        let cfg = ServerConfig::from(
            params.clone(),
            params.identity,
            broadcast_public_keys,
            broadcast_sk,
            mint,
            ln,
            wallet,
            code_version_str,
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

    pub fn iroh_endpoints(&self) -> BTreeMap<PeerId, PeerIrohEndpoints> {
        self.peers
            .iter()
            .map(|(id, peer)| {
                let endpoints = PeerIrohEndpoints {
                    name: peer.name.clone(),
                    api_pk: peer.endpoints.api_pk,
                    p2p_pk: peer.endpoints.p2p_pk,
                };
                (*id, endpoints)
            })
            .collect()
    }
}
