pub mod bitcoind;
pub mod esplora;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use anyhow::{Result, ensure};
use picomint_core::bitcoin::{Block, BlockHash, Network, Transaction};
use picomint_core::task::TaskGroup;
use picomint_core::util::{SafeUrl};
use picomint_core::ChainId;
use picomint_logging::LOG_SERVER;
use tokio::sync::watch;
use tracing::{debug, warn};

pub use crate::bitcoind::BitcoindClient;
pub use crate::esplora::EsploraClient;

// Well-known genesis block hashes for different Bitcoin networks
// <https://blockstream.info/api/block-height/1>
const MAINNET_CHAIN_ID_STR: &str =
    "00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048";
// <https://blockstream.info/testnet/api/block-height/1>
const TESTNET_CHAIN_ID_STR: &str =
    "00000000b873e79784647a6c82962c70d228557d24a747ea4d1b8bbe878e1206";
// <https://mempool.space/signet/api/block-height/1>
const SIGNET_4_CHAIN_ID_STR: &str =
    "00000086d6b2636cb2a392d45edc4ec544a10024d30141c9adf4bfd9de533b53";
// <https://mutinynet.com/api/block-height/1>
const MUTINYNET_CHAIN_ID_STR: &str =
    "000002855893a0a9b24eaffc5efc770558a326fee4fc10c9da22fc19cd2954f9";

/// Derives the Bitcoin network from a chain ID (block height 1 block hash).
///
/// Returns the corresponding `Network` for well-known genesis hashes,
/// or `Network::Regtest` for unknown hashes (custom/private networks).
pub fn network_from_chain_id(chain_id: ChainId) -> Network {
    match chain_id.to_string().as_str() {
        MAINNET_CHAIN_ID_STR => Network::Bitcoin,
        TESTNET_CHAIN_ID_STR => Network::Testnet,
        SIGNET_4_CHAIN_ID_STR => Network::Signet,
        MUTINYNET_CHAIN_ID_STR => Network::Signet,
        _ => Network::Regtest,
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Feerate {
    pub sats_per_kvb: u64,
}

/// Status of the Bitcoin RPC backend as reported by the monitor.
#[derive(Debug, Clone)]
pub struct BitcoinRpcStatus {
    pub network: Network,
    pub block_count: u64,
    pub fee_rate: Feerate,
    pub sync_progress: Option<f64>,
}

/// Match-dispatched backend over the two concrete RPC clients.
#[derive(Debug)]
pub enum BitcoinBackend {
    Bitcoind(BitcoindClient),
    Esplora(EsploraClient),
}

impl BitcoinBackend {
    pub fn url(&self) -> SafeUrl {
        match self {
            BitcoinBackend::Bitcoind(c) => c.url(),
            BitcoinBackend::Esplora(c) => c.url(),
        }
    }

    pub async fn get_block_count(&self) -> Result<u64> {
        match self {
            BitcoinBackend::Bitcoind(c) => c.get_block_count().await,
            BitcoinBackend::Esplora(c) => c.get_block_count().await,
        }
    }

    pub async fn get_block_hash(&self, height: u64) -> Result<BlockHash> {
        match self {
            BitcoinBackend::Bitcoind(c) => c.get_block_hash(height).await,
            BitcoinBackend::Esplora(c) => c.get_block_hash(height).await,
        }
    }

    pub async fn get_block(&self, hash: &BlockHash) -> Result<Block> {
        match self {
            BitcoinBackend::Bitcoind(c) => c.get_block(hash).await,
            BitcoinBackend::Esplora(c) => c.get_block(hash).await,
        }
    }

    pub async fn get_feerate(&self) -> Result<Option<Feerate>> {
        match self {
            BitcoinBackend::Bitcoind(c) => c.get_feerate().await,
            BitcoinBackend::Esplora(c) => c.get_feerate().await,
        }
    }

    pub async fn submit_transaction(&self, transaction: Transaction) {
        match self {
            BitcoinBackend::Bitcoind(c) => c.submit_transaction(transaction).await,
            BitcoinBackend::Esplora(c) => c.submit_transaction(transaction).await,
        }
    }

    pub async fn get_sync_progress(&self) -> Result<Option<f64>> {
        match self {
            BitcoinBackend::Bitcoind(c) => c.get_sync_progress().await,
            BitcoinBackend::Esplora(c) => c.get_sync_progress().await,
        }
    }

    pub async fn get_chain_id(&self) -> Result<ChainId> {
        match self {
            BitcoinBackend::Bitcoind(c) => c.get_chain_id().await,
            BitcoinBackend::Esplora(c) => c.get_chain_id().await,
        }
    }
}

#[derive(Debug)]
pub struct BitcoinRpcMonitor {
    rpc: Arc<BitcoinBackend>,
    status_receiver: watch::Receiver<Option<BitcoinRpcStatus>>,
    /// Cached chain ID — fetched once and never changes.
    chain_id: OnceLock<ChainId>,
}

impl BitcoinRpcMonitor {
    pub fn new(
        rpc: Arc<BitcoinBackend>,
        update_interval: Duration,
        task_group: &TaskGroup,
    ) -> Self {
        let (status_sender, status_receiver) = watch::channel(None);

        let rpc_clone = rpc.clone();
        debug!(
            target: LOG_SERVER,
            interval_ms  = %update_interval.as_millis(),
            "Starting bitcoin rpc monitor"
        );

        task_group.spawn_cancellable("bitcoin-status-update", async move {
            let mut interval = tokio::time::interval(update_interval);
            loop {
                interval.tick().await;
                match Self::fetch_status(&rpc_clone).await {
                    Ok(new_status) => {
                        status_sender.send_replace(Some(new_status));
                    }
                    Err(err) => {
                        warn!(
                            target: LOG_SERVER,
                            err = %format_args!("{err:#}"),
                            "Bitcoin status update failed"
                        );
                        status_sender.send_replace(None);
                    }
                }
            }
        });

        Self {
            rpc,
            status_receiver,
            chain_id: OnceLock::new(),
        }
    }

    async fn fetch_status(rpc: &BitcoinBackend) -> Result<BitcoinRpcStatus> {
        let chain_id = rpc.get_chain_id().await?;
        let network = network_from_chain_id(chain_id);
        let block_count = rpc.get_block_count().await?;
        let sync_progress = rpc.get_sync_progress().await?;

        let fee_rate = if network == Network::Regtest {
            Feerate { sats_per_kvb: 1000 }
        } else {
            rpc.get_feerate()
                .await?
                .ok_or_else(|| anyhow::anyhow!("Feerate not available"))?
        };

        Ok(BitcoinRpcStatus {
            network,
            block_count,
            fee_rate,
            sync_progress,
        })
    }

    pub fn url(&self) -> SafeUrl {
        self.rpc.url()
    }

    pub fn status(&self) -> Option<BitcoinRpcStatus> {
        self.status_receiver.borrow().clone()
    }

    pub async fn get_block(&self, hash: &BlockHash) -> Result<Block> {
        ensure!(
            self.status_receiver.borrow().is_some(),
            "Not connected to bitcoin backend"
        );

        self.rpc.get_block(hash).await
    }

    pub async fn get_block_hash(&self, height: u64) -> Result<BlockHash> {
        ensure!(
            self.status_receiver.borrow().is_some(),
            "Not connected to bitcoin backend"
        );

        self.rpc.get_block_hash(height).await
    }

    pub async fn submit_transaction(&self, tx: Transaction) {
        if self.status_receiver.borrow().is_some() {
            self.rpc.submit_transaction(tx).await;
        }
    }
}

impl Clone for BitcoinRpcMonitor {
    fn clone(&self) -> Self {
        Self {
            rpc: self.rpc.clone(),
            status_receiver: self.status_receiver.clone(),
            chain_id: self
                .chain_id
                .get()
                .copied()
                .map(|h| {
                    let lock = OnceLock::new();
                    let _ = lock.set(h);
                    lock
                })
                .unwrap_or_default(),
        }
    }
}
