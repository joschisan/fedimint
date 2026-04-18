use std::collections::HashMap;

use anyhow::Context;
use bitcoin::{BlockHash, Transaction};
use picomint_core::util::SafeUrl;

use crate::Feerate;
use picomint_logging::{LOG_BITCOIND_ESPLORA, LOG_SERVER};
use tracing::info;

#[derive(Debug)]
pub struct EsploraClient {
    client: esplora_client::AsyncClient,
    url: SafeUrl,
}

impl EsploraClient {
    pub fn new(url: &SafeUrl) -> anyhow::Result<Self> {
        info!(
            target: LOG_SERVER,
            %url,
            "Initializing bitcoin esplora backend"
        );
        // URL needs to have any trailing path including '/' removed
        let without_trailing = url.as_str().trim_end_matches('/');

        let builder = esplora_client::Builder::new(without_trailing);
        let client = builder.build_async()?;
        Ok(Self {
            client,
            url: url.clone(),
        })
    }

    pub fn url(&self) -> SafeUrl {
        self.url.clone()
    }

    pub async fn get_block_count(&self) -> anyhow::Result<u64> {
        match self.client.get_height().await {
            Ok(height) => Ok(u64::from(height) + 1),
            Err(e) => Err(e.into()),
        }
    }

    pub async fn get_block_hash(&self, height: u64) -> anyhow::Result<BlockHash> {
        Ok(self.client.get_block_hash(u32::try_from(height)?).await?)
    }

    pub async fn get_block(&self, block_hash: &BlockHash) -> anyhow::Result<bitcoin::Block> {
        self.client
            .get_block_by_hash(block_hash)
            .await?
            .context("Block with this hash is not available")
    }

    pub async fn get_feerate(&self) -> anyhow::Result<Option<Feerate>> {
        let fee_estimates: HashMap<u16, f64> = self.client.get_fee_estimates().await?;

        let fee_rate_vb = esplora_client::convert_fee_rate(1, fee_estimates).unwrap_or(1.0);

        let fee_rate_kvb = fee_rate_vb * 1_000f32;

        Ok(Some(Feerate {
            sats_per_kvb: (fee_rate_kvb).ceil() as u64,
        }))
    }

    pub async fn submit_transaction(&self, transaction: Transaction) {
        let _ = self.client.broadcast(&transaction).await.map_err(|err| {
            info!(target: LOG_BITCOIND_ESPLORA, err = %err, "Error broadcasting transaction");
        });
    }

    pub async fn get_sync_progress(&self) -> anyhow::Result<Option<f64>> {
        Ok(None)
    }
}
