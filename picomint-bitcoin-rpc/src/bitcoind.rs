use anyhow::anyhow;
use bitcoin::{BlockHash, Transaction};
use bitcoincore_rpc::Error::JsonRpc;
use bitcoincore_rpc::bitcoincore_rpc_json::EstimateMode;
use bitcoincore_rpc::jsonrpc::Error::Rpc;
use bitcoincore_rpc::{Auth, Client, RpcApi};
use picomint_core::runtime::block_in_place;
use picomint_core::util::{SafeUrl};
use picomint_core::{ChainId, Feerate};
use picomint_logging::{LOG_BITCOIND_CORE, LOG_SERVER};
use tracing::info;

#[derive(Debug)]
pub struct BitcoindClient {
    client: Client,
    url: SafeUrl,
}

impl BitcoindClient {
    pub fn new(username: String, password: String, url: &SafeUrl) -> anyhow::Result<Self> {
        let auth = Auth::UserPass(username, password);

        let url = url
            .without_auth()
            .map_err(|()| anyhow!("Failed to strip auth from Bitcoin Rpc Url"))?;

        info!(
            target: LOG_SERVER,
            %url,
            "Initializing bitcoin bitcoind backend"
        );
        Ok(Self {
            client: Client::new(url.as_str(), auth)?,
            url,
        })
    }

    pub fn url(&self) -> SafeUrl {
        self.url.clone()
    }

    pub async fn get_block_count(&self) -> anyhow::Result<u64> {
        // The RPC function is confusingly named and actually returns the block height
        block_in_place(|| self.client.get_block_count())
            .map(|height| height + 1)
            .map_err(anyhow::Error::from)
    }

    pub async fn get_block_hash(&self, height: u64) -> anyhow::Result<BlockHash> {
        block_in_place(|| self.client.get_block_hash(height)).map_err(anyhow::Error::from)
    }

    pub async fn get_block(&self, hash: &BlockHash) -> anyhow::Result<bitcoin::Block> {
        block_in_place(|| self.client.get_block(hash)).map_err(anyhow::Error::from)
    }

    pub async fn get_feerate(&self) -> anyhow::Result<Option<Feerate>> {
        let feerate = block_in_place(|| {
            self.client
                .estimate_smart_fee(1, Some(EstimateMode::Conservative))
        })?
        .fee_rate
        .map(|per_kb| Feerate {
            sats_per_kvb: per_kb.to_sat(),
        });

        Ok(feerate)
    }

    pub async fn submit_transaction(&self, transaction: Transaction) {
        match block_in_place(|| self.client.send_raw_transaction(&transaction)) {
            // Bitcoin core's RPC will return error code -27 if a transaction is already in a block.
            // This is considered a success case, so we don't surface the error log.
            //
            // https://github.com/bitcoin/bitcoin/blob/daa56f7f665183bcce3df146f143be37f33c123e/src/rpc/protocol.h#L48
            Err(JsonRpc(Rpc(e))) if e.code == -27 => (),
            Err(e) => {
                info!(target: LOG_BITCOIND_CORE, e = %e, "Error broadcasting transaction")
            }
            Ok(_) => (),
        }
    }

    pub async fn get_sync_progress(&self) -> anyhow::Result<Option<f64>> {
        Ok(Some(
            block_in_place(|| self.client.get_blockchain_info())?.verification_progress,
        ))
    }

    pub async fn get_chain_id(&self) -> anyhow::Result<ChainId> {
        self.get_block_hash(1).await.map(ChainId::new)
    }
}
