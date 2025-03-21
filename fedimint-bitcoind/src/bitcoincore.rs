use std::env;
use std::path::PathBuf;

use anyhow::{anyhow as format_err, bail};
use bitcoin::{BlockHash, Network, ScriptBuf, Transaction, Txid};
use bitcoincore_rpc::bitcoincore_rpc_json::EstimateMode;
use bitcoincore_rpc::{Auth, RpcApi};
use fedimint_core::encoding::Decodable;
use fedimint_core::envs::{BitcoinRpcConfig, FM_BITCOIND_COOKIE_FILE_ENV};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::runtime::block_in_place;
use fedimint_core::txoproof::TxOutProof;
use fedimint_core::util::SafeUrl;
use fedimint_core::{Feerate, apply, async_trait_maybe_send};
use fedimint_logging::{LOG_BITCOIND_CORE, LOG_CORE};
use tracing::{info, warn};

use crate::{DynBitcoindRpc, IBitcoindRpc, IBitcoindRpcFactory};

#[derive(Debug)]
pub struct BitcoindFactory;

impl IBitcoindRpcFactory for BitcoindFactory {
    fn create_connection(&self, url: &SafeUrl) -> anyhow::Result<DynBitcoindRpc> {
        Ok(BitcoindClient::new(url)?.into())
    }
}

#[derive(Debug)]
struct BitcoindClient {
    client: ::bitcoincore_rpc::Client,
    url: SafeUrl,
}

impl BitcoindClient {
    fn new(url: &SafeUrl) -> anyhow::Result<Self> {
        let safe_url = url.clone();
        let (url, auth) = from_url_to_url_auth(url)?;
        Ok(Self {
            client: ::bitcoincore_rpc::Client::new(&url, auth)?,
            url: safe_url,
        })
    }
}

#[apply(async_trait_maybe_send!)]
impl IBitcoindRpc for BitcoindClient {
    async fn get_network(&self) -> anyhow::Result<Network> {
        let network = block_in_place(|| self.client.get_blockchain_info())?;
        Ok(network.chain)
    }

    async fn get_block_count(&self) -> anyhow::Result<u64> {
        // The RPC function is confusingly named and actually returns the block height
        block_in_place(|| self.client.get_block_count())
            .map(|height| height + 1)
            .map_err(anyhow::Error::from)
    }

    async fn get_block_hash(&self, height: u64) -> anyhow::Result<BlockHash> {
        block_in_place(|| self.client.get_block_hash(height)).map_err(anyhow::Error::from)
    }

    async fn get_block(&self, hash: &BlockHash) -> anyhow::Result<bitcoin::Block> {
        block_in_place(|| self.client.get_block(hash)).map_err(anyhow::Error::from)
    }

    async fn get_fee_rate(&self, confirmation_target: u16) -> anyhow::Result<Option<Feerate>> {
        let fee = block_in_place(|| {
            self.client
                .estimate_smart_fee(confirmation_target, Some(EstimateMode::Conservative))
        });
        Ok(fee?.fee_rate.map(|per_kb| Feerate {
            sats_per_kvb: per_kb.to_sat(),
        }))
    }

    async fn submit_transaction(&self, transaction: Transaction) {
        use bitcoincore_rpc::Error::JsonRpc;
        use bitcoincore_rpc::jsonrpc::Error::Rpc;
        match block_in_place(|| self.client.send_raw_transaction(&transaction)) {
            // Bitcoin core's RPC will return error code -27 if a transaction is already in a block.
            // This is considered a success case, so we don't surface the error log.
            //
            // https://github.com/bitcoin/bitcoin/blob/daa56f7f665183bcce3df146f143be37f33c123e/src/rpc/protocol.h#L48
            Err(JsonRpc(Rpc(e))) if e.code == -27 => (),
            Err(e) => info!(target: LOG_BITCOIND_CORE, ?e, "Error broadcasting transaction"),
            Ok(_) => (),
        }
    }

    async fn get_tx_block_height(&self, txid: &Txid) -> anyhow::Result<Option<u64>> {
        let info = block_in_place(|| self.client.get_raw_transaction_info(txid, None)).map_err(
            |error| info!(target: LOG_BITCOIND_CORE, ?error, "Unable to get raw transaction"),
        );
        let height = match info.ok().and_then(|info| info.blockhash) {
            None => None,
            Some(hash) => Some(block_in_place(|| self.client.get_block_header_info(&hash))?.height),
        };
        Ok(height.map(|h| h as u64))
    }

    async fn is_tx_in_block(
        &self,
        txid: &Txid,
        block_hash: &BlockHash,
        block_height: u64,
    ) -> anyhow::Result<bool> {
        let block_info = block_in_place(|| self.client.get_block_info(block_hash))?;
        anyhow::ensure!(
            block_info.height as u64 == block_height,
            "Block height for block hash does not match expected height"
        );
        Ok(block_info.tx.contains(txid))
    }

    async fn watch_script_history(&self, script: &ScriptBuf) -> anyhow::Result<()> {
        warn!(target: LOG_CORE, "Wallet operations are broken on bitcoind. Use different backend.");
        // start watching for this script in our wallet to avoid the need to rescan the
        // blockchain, labeling it so we can reference it later
        block_in_place(|| {
            self.client
                .import_address_script(script, Some(&script.to_string()), Some(false), None)
        })?;

        Ok(())
    }

    async fn get_script_history(&self, script: &ScriptBuf) -> anyhow::Result<Vec<Transaction>> {
        let mut results = vec![];
        let list = block_in_place(|| {
            self.client
                .list_transactions(Some(&script.to_string()), None, None, Some(true))
        })?;
        for tx in list {
            let raw_tx = block_in_place(|| self.client.get_raw_transaction(&tx.info.txid, None))?;
            results.push(raw_tx);
        }
        Ok(results)
    }

    async fn get_txout_proof(&self, txid: Txid) -> anyhow::Result<TxOutProof> {
        TxOutProof::consensus_decode_whole(
            &block_in_place(|| self.client.get_tx_out_proof(&[txid], None))?,
            &ModuleDecoderRegistry::default(),
        )
        .map_err(|error| format_err!("Could not decode tx: {}", error))
    }

    async fn get_sync_percentage(&self) -> anyhow::Result<Option<f64>> {
        let blockchain_info = block_in_place(|| self.client.get_blockchain_info())?;
        Ok(Some(blockchain_info.verification_progress))
    }

    fn get_bitcoin_rpc_config(&self) -> BitcoinRpcConfig {
        BitcoinRpcConfig {
            kind: "bitcoind".to_string(),
            url: self.url.clone(),
        }
    }
}

// TODO: Make private
pub fn from_url_to_url_auth(url: &SafeUrl) -> anyhow::Result<(String, Auth)> {
    Ok((
        (if let Some(port) = url.port() {
            format!(
                "{}://{}:{port}",
                url.scheme(),
                url.host_str().unwrap_or("127.0.0.1")
            )
        } else {
            format!(
                "{}://{}",
                url.scheme(),
                url.host_str().unwrap_or("127.0.0.1")
            )
        }),
        match (
            !url.username().is_empty(),
            env::var(FM_BITCOIND_COOKIE_FILE_ENV),
        ) {
            (true, Ok(_)) => {
                bail!("When {FM_BITCOIND_COOKIE_FILE_ENV} is set, the url auth part must be empty.")
            }
            (true, Err(_)) => Auth::UserPass(
                url.username().to_owned(),
                url.password()
                    .ok_or_else(|| format_err!("Password missing for {}", url.username()))?
                    .to_owned(),
            ),
            (false, Ok(path)) => Auth::CookieFile(PathBuf::from(path)),
            (false, Err(_)) => Auth::None,
        },
    ))
}
