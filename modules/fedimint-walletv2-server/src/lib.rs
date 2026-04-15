#![deny(clippy::pedantic)]
#![allow(clippy::similar_names)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::default_trait_access)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::single_match_else)]
#![allow(clippy::too_many_lines)]

pub mod db;

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, anyhow, bail, ensure};
use bitcoin::absolute::LockTime;
use bitcoin::hashes::{Hash, sha256};
use bitcoin::secp256k1::Secp256k1;
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Amount, Network, Sequence, Transaction, TxIn, TxOut, Txid};
use common::config::WalletConfigConsensus;
use common::{
    OutputInfo, WalletCommonInit, WalletConsensusItem, WalletInput, WalletModuleTypes, WalletOutput,
};
use db::{
    BLOCK_COUNT_VOTE, FEDERATION_WALLET, FEE_RATE_VOTE, OUTPUT, Output, SIGNATURES, SPENT_OUTPUT,
    TX_INFO, TX_INFO_INDEX, UNCONFIRMED_TX, UNSIGNED_TX,
};
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{
    IReadDatabaseTransactionOps, IReadDatabaseTransactionOpsTyped as _,
    IWriteDatabaseTransactionOpsTyped as _,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    ApiEndpoint, ApiVersion, CoreConsensusVersion, InputMeta, ModuleConsensusVersion, ModuleInit,
    TransactionItemAmounts, api_endpoint,
};
#[cfg(not(target_family = "wasm"))]
use fedimint_core::task::TaskGroup;
use fedimint_core::task::sleep;
use fedimint_core::{InPoint, NumPeersExt, OutPoint, PeerId, apply, async_trait_maybe_send, util};
use fedimint_logging::LOG_MODULE_WALLETV2;
use fedimint_redb::{Database, ReadTxRef, WriteTxRef};
use fedimint_server_core::bitcoin_rpc::ServerBitcoinRpcMonitor;
use fedimint_server_core::config::{PeerHandleOps, PeerHandleOpsExt};
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
pub use fedimint_walletv2_common as common;
use fedimint_walletv2_common::config::{WalletClientConfig, WalletConfig, WalletConfigPrivate};
use fedimint_walletv2_common::endpoint_constants::{
    CONSENSUS_BLOCK_COUNT_ENDPOINT, CONSENSUS_FEERATE_ENDPOINT, FEDERATION_WALLET_ENDPOINT,
    OUTPUT_INFO_SLICE_ENDPOINT, PENDING_TRANSACTION_CHAIN_ENDPOINT, RECEIVE_FEE_ENDPOINT,
    SEND_FEE_ENDPOINT, TRANSACTION_CHAIN_ENDPOINT, TRANSACTION_ID_ENDPOINT,
};
use fedimint_walletv2_common::{
    FederationWallet, MODULE_CONSENSUS_VERSION, TxInfo, WalletInputError, WalletOutputError,
    descriptor, is_potential_receive, tweak_public_key,
};
use miniscript::descriptor::Wsh;
use rand::rngs::OsRng;
use secp256k1::ecdsa::Signature;
use secp256k1::{PublicKey, Scalar};
use serde::{Deserialize, Serialize};
use tracing::info;

/// Number of confirmations required for a transaction to be considered as
/// final by the federation. The block that mines the transaction does
/// not count towards the number of confirmations.
pub const CONFIRMATION_FINALITY_DELAY: u64 = 6;

/// Maximum number of blocks the consensus block count can advance in a single
/// consensus item to limit the work done in one `process_consensus_item` step.
const MAX_BLOCK_COUNT_INCREMENT: u64 = 10;

/// Minimum fee rate vote of 1 sat/vB to ensure we never propose a fee rate
/// below what Bitcoin Core will relay.
const MIN_FEERATE_VOTE_SATS_PER_KVB: u64 = 1000;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct FederationTx {
    pub tx: Transaction,
    pub spent_tx_outs: Vec<SpentTxOut>,
    pub vbytes: u64,
    pub fee: Amount,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct SpentTxOut {
    pub value: Amount,
    pub tweak: sha256::Hash,
}

fn pending_txs_unordered(dbtx: &WriteTxRef<'_>) -> Vec<FederationTx> {
    let unsigned: Vec<FederationTx> = dbtx
        .iter(&UNSIGNED_TX)
        .into_iter()
        .map(|(_, v)| v)
        .collect();

    let unconfirmed: Vec<FederationTx> = dbtx
        .iter(&UNCONFIRMED_TX)
        .into_iter()
        .map(|(_, v)| v)
        .collect();

    unsigned.into_iter().chain(unconfirmed).collect()
}

#[derive(Debug, Clone)]
pub struct WalletInit;

impl ModuleInit for WalletInit {
    type Common = WalletCommonInit;
}

#[apply(async_trait_maybe_send!)]
impl ServerModuleInit for WalletInit {
    type Module = Wallet;

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(Wallet::new(
            args.cfg().to_typed()?,
            args.db(),
            args.task_group(),
            args.server_bitcoin_rpc_monitor(),
        ))
    }

    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        let (bitcoin_sk, bitcoin_pk) = secp256k1::generate_keypair(&mut OsRng);

        let bitcoin_pks: BTreeMap<PeerId, PublicKey> = peers
            .exchange_encodable(bitcoin_pk)
            .await?
            .into_iter()
            .collect();

        let config = WalletConfig {
            private: WalletConfigPrivate { bitcoin_sk },
            consensus: WalletConfigConsensus::new(bitcoin_pks, args.network),
        };

        Ok(config.to_erased())
    }

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()> {
        let config = config.to_typed::<WalletConfig>()?;

        ensure!(
            config
                .consensus
                .bitcoin_pks
                .get(identity)
                .ok_or(anyhow::anyhow!("No public key for our identity"))?
                == &config.private.bitcoin_sk.public_key(secp256k1::SECP256K1),
            "Bitcoin wallet private key doesn't match multisig pubkey"
        );

        Ok(())
    }

    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<WalletClientConfig> {
        let config = WalletConfigConsensus::from_erased(config)?;

        Ok(WalletClientConfig {
            bitcoin_pks: config.bitcoin_pks,
            send_tx_vbytes: config.send_tx_vbytes,
            receive_tx_vbytes: config.receive_tx_vbytes,
            feerate_base: config.feerate_base,
            dust_limit: config.dust_limit,
            input_fee: config.input_fee,
            output_fee: config.output_fee,
            network: config.network,
        })
    }
}

#[apply(async_trait_maybe_send!)]
impl ServerModule for Wallet {
    type Common = WalletModuleTypes;
    type Init = WalletInit;

    async fn consensus_proposal(&self, dbtx: &ReadTxRef<'_>) -> Vec<WalletConsensusItem> {
        let mut items: Vec<WalletConsensusItem> = dbtx
            .iter(&UNSIGNED_TX)
            .into_iter()
            .map(|(txid, unsigned_tx)| {
                let signatures = self.sign_tx(&unsigned_tx);

                self.verify_signatures(
                    &unsigned_tx,
                    &signatures,
                    self.cfg.private.bitcoin_sk.public_key(secp256k1::SECP256K1),
                )
                .expect("Our signatures failed verification against our private key");

                WalletConsensusItem::Signatures(txid, signatures)
            })
            .collect();

        if let Some(status) = self.btc_rpc.status() {
            assert_eq!(status.network, self.cfg.consensus.network);

            let block_count_vote = status
                .block_count
                .saturating_sub(CONFIRMATION_FINALITY_DELAY);

            let consensus_block_count = self.consensus_block_count(dbtx);

            let block_count_vote = match consensus_block_count {
                0 => block_count_vote,
                _ => block_count_vote.min(consensus_block_count + MAX_BLOCK_COUNT_INCREMENT),
            };

            items.push(WalletConsensusItem::BlockCount(block_count_vote));

            let feerate_vote = status
                .fee_rate
                .sats_per_kvb
                .max(MIN_FEERATE_VOTE_SATS_PER_KVB);

            items.push(WalletConsensusItem::Feerate(Some(feerate_vote)));
        } else {
            // Bitcoin backend not connected, retract fee rate vote
            items.push(WalletConsensusItem::Feerate(None));
        }

        items
    }

    async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: WalletConsensusItem,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        match consensus_item {
            WalletConsensusItem::BlockCount(block_count_vote) => {
                self.process_block_count(dbtx, block_count_vote, peer).await
            }
            WalletConsensusItem::Feerate(feerate) => {
                if Some(feerate) == dbtx.insert(&FEE_RATE_VOTE, &peer, &feerate) {
                    return Err(anyhow!("Fee rate vote is redundant"));
                }

                Ok(())
            }
            WalletConsensusItem::Signatures(txid, signatures) => {
                self.process_signatures(dbtx, txid, signatures, peer).await
            }
            WalletConsensusItem::Default { variant, .. } => Err(anyhow!(
                "Received wallet consensus item with unknown variant {variant}"
            )),
        }
    }

    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &WalletInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, WalletInputError> {
        let input = input.ensure_v0_ref()?;

        if dbtx
            .insert(&SPENT_OUTPUT, &input.output_index, &())
            .is_some()
        {
            return Err(WalletInputError::OutputAlreadySpent);
        }

        let Output(tracked_outpoint, tracked_output) = dbtx
            .get(&OUTPUT, &input.output_index)
            .ok_or(WalletInputError::UnknownOutputIndex)?;

        let tweaked_pubkey = self
            .descriptor(&input.tweak.consensus_hash())
            .script_pubkey();

        if tracked_output.script_pubkey != tweaked_pubkey {
            return Err(WalletInputError::WrongTweak);
        }

        let consensus_receive_fee = self
            .receive_fee(dbtx)
            .ok_or(WalletInputError::NoConsensusFeerateAvailable)?;

        // We allow for a higher fee such that a guardian could construct a CPFP
        // transaction. This is the last line of defense should the federations
        // transactions ever get stuck due to a critical failure of the feerate
        // estimation.
        if input.fee < consensus_receive_fee {
            return Err(WalletInputError::InsufficientTotalFee);
        }

        let output_value = tracked_output
            .value
            .checked_sub(input.fee)
            .ok_or(WalletInputError::ArithmeticOverflow)?;

        if let Some(wallet) = dbtx.remove(&FEDERATION_WALLET, &()) {
            // Assuming the first receive into the federation is made through a
            // standard transaction, its output value is over the P2WSH dust
            // limit. By induction so is this change value.
            let change_value = wallet
                .value
                .checked_add(output_value)
                .ok_or(WalletInputError::ArithmeticOverflow)?;

            let tx = Transaction {
                version: Version(2),
                lock_time: LockTime::ZERO,
                input: vec![
                    TxIn {
                        previous_output: wallet.outpoint,
                        script_sig: Default::default(),
                        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                        witness: bitcoin::Witness::new(),
                    },
                    TxIn {
                        previous_output: tracked_outpoint,
                        script_sig: Default::default(),
                        sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                        witness: bitcoin::Witness::new(),
                    },
                ],
                output: vec![TxOut {
                    value: change_value,
                    script_pubkey: self.descriptor(&wallet.consensus_hash()).script_pubkey(),
                }],
            };

            dbtx.insert(
                &FEDERATION_WALLET,
                &(),
                &FederationWallet {
                    value: change_value,
                    outpoint: bitcoin::OutPoint {
                        txid: tx.compute_txid(),
                        vout: 0,
                    },
                    tweak: wallet.consensus_hash(),
                },
            );

            let tx_index = self.total_txs(dbtx);

            let created = self.consensus_block_count(dbtx);

            dbtx.insert(
                &TX_INFO,
                &tx_index,
                &TxInfo {
                    index: tx_index,
                    txid: tx.compute_txid(),
                    input: wallet.value,
                    output: change_value,
                    vbytes: self.cfg.consensus.receive_tx_vbytes,
                    fee: input.fee,
                    created,
                },
            );

            dbtx.insert(
                &UNSIGNED_TX,
                &tx.compute_txid(),
                &FederationTx {
                    tx: tx.clone(),
                    spent_tx_outs: vec![
                        SpentTxOut {
                            value: wallet.value,
                            tweak: wallet.tweak,
                        },
                        SpentTxOut {
                            value: tracked_output.value,
                            tweak: input.tweak.consensus_hash(),
                        },
                    ],
                    vbytes: self.cfg.consensus.receive_tx_vbytes,
                    fee: input.fee,
                },
            );
        } else {
            dbtx.insert(
                &FEDERATION_WALLET,
                &(),
                &FederationWallet {
                    value: tracked_output.value,
                    outpoint: tracked_outpoint,
                    tweak: input.tweak.consensus_hash(),
                },
            );
        }

        let amount = output_value
            .to_sat()
            .checked_mul(1000)
            .map(fedimint_core::Amount::from_msats)
            .ok_or(WalletInputError::ArithmeticOverflow)?;

        Ok(InputMeta {
            amount: TransactionItemAmounts {
                amount,
                fee: self.cfg.consensus.input_fee,
            },
            pub_key: input.tweak,
        })
    }

    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &WalletOutput,
        outpoint: OutPoint,
    ) -> Result<TransactionItemAmounts, WalletOutputError> {
        let output = output.ensure_v0_ref()?;

        if output.value < self.cfg.consensus.dust_limit {
            return Err(WalletOutputError::UnderDustLimit);
        }

        let wallet = dbtx
            .remove(&FEDERATION_WALLET, &())
            .ok_or(WalletOutputError::NoFederationUTXO)?;

        let consensus_send_fee = self
            .send_fee(dbtx)
            .ok_or(WalletOutputError::NoConsensusFeerateAvailable)?;

        // We allow for a higher fee such that a guardian could construct a CPFP
        // transaction. This is the last line of defense should the federations
        // transactions ever get stuck due to a critical failure of the feerate
        // estimation.
        if output.fee < consensus_send_fee {
            return Err(WalletOutputError::InsufficientTotalFee);
        }

        let output_value = output
            .value
            .checked_add(output.fee)
            .ok_or(WalletOutputError::ArithmeticOverflow)?;

        let change_value = wallet
            .value
            .checked_sub(output_value)
            .ok_or(WalletOutputError::ArithmeticOverflow)?;

        if change_value < self.cfg.consensus.dust_limit {
            return Err(WalletOutputError::ChangeUnderDustLimit);
        }

        let script_pubkey = output
            .destination
            .script_pubkey()
            .ok_or(WalletOutputError::UnknownScriptVariant)?;

        let tx = Transaction {
            version: Version(2),
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: wallet.outpoint,
                script_sig: Default::default(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: change_value,
                    script_pubkey: self.descriptor(&wallet.consensus_hash()).script_pubkey(),
                },
                TxOut {
                    value: output.value,
                    script_pubkey,
                },
            ],
        };

        dbtx.insert(
            &FEDERATION_WALLET,
            &(),
            &FederationWallet {
                value: change_value,
                outpoint: bitcoin::OutPoint {
                    txid: tx.compute_txid(),
                    vout: 0,
                },
                tweak: wallet.consensus_hash(),
            },
        );

        let tx_index = self.total_txs(dbtx);

        let created = self.consensus_block_count(dbtx);

        dbtx.insert(
            &TX_INFO,
            &tx_index,
            &TxInfo {
                index: tx_index,
                txid: tx.compute_txid(),
                input: wallet.value,
                output: change_value,
                vbytes: self.cfg.consensus.send_tx_vbytes,
                fee: output.fee,
                created,
            },
        );

        dbtx.insert(&TX_INFO_INDEX, &outpoint, &tx_index);

        dbtx.insert(
            &UNSIGNED_TX,
            &tx.compute_txid(),
            &FederationTx {
                tx: tx.clone(),
                spent_tx_outs: vec![SpentTxOut {
                    value: wallet.value,
                    tweak: wallet.tweak,
                }],
                vbytes: self.cfg.consensus.send_tx_vbytes,
                fee: output.fee,
            },
        );

        let amount = output_value
            .to_sat()
            .checked_mul(1000)
            .map(fedimint_core::Amount::from_msats)
            .ok_or(WalletOutputError::ArithmeticOverflow)?;

        Ok(TransactionItemAmounts {
            amount,
            fee: self.cfg.consensus.output_fee,
        })
    }

    async fn audit(
        &self,
        dbtx: &WriteTxRef<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    ) {
        let items = dbtx
            .iter(&FEDERATION_WALLET)
            .into_iter()
            .map(|(_, wallet)| {
                (
                    "FederationWallet".to_string(),
                    1000 * wallet.value.to_sat() as i64,
                )
            });

        audit.add_items(module_instance_id, items);
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            api_endpoint! {
                CONSENSUS_BLOCK_COUNT_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> u64 {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.consensus_block_count(&dbtx.as_ref());
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                CONSENSUS_FEERATE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> Option<u64> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.consensus_feerate(&dbtx.as_ref());
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                FEDERATION_WALLET_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> Option<FederationWallet> {
                    let db = module.db.clone();
                    let tx = db.begin_read().await;
                    Ok(tx.get(&FEDERATION_WALLET, &()))
                }
            },
            api_endpoint! {
                SEND_FEE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> Option<Amount> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.send_fee(&dbtx.as_ref());
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                RECEIVE_FEE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> Option<Amount> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.receive_fee(&dbtx.as_ref());
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                TRANSACTION_ID_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, params: OutPoint| -> Option<Txid> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.tx_id(&dbtx.as_ref(), params);
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                OUTPUT_INFO_SLICE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, params: (u64, u64)| -> Vec<OutputInfo> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.get_outputs(&dbtx.as_ref(), params.0, params.1);
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                PENDING_TRANSACTION_CHAIN_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> Vec<TxInfo> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.pending_tx_chain(&dbtx.as_ref());
                    dbtx.commit().await;
                    Ok(result)
                }
            },
            api_endpoint! {
                TRANSACTION_CHAIN_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _params: ()| -> Vec<TxInfo> {
                    let db = module.db.clone();
                    let dbtx = db.begin_write().await;
                    let result = module.tx_chain(&dbtx.as_ref());
                    dbtx.commit().await;
                    Ok(result)
                }
            },
        ]
    }
}

#[derive(Debug)]
pub struct Wallet {
    cfg: WalletConfig,
    db: Database,
    btc_rpc: ServerBitcoinRpcMonitor,
}

impl Wallet {
    fn new(
        cfg: WalletConfig,
        db: &Database,
        task_group: &TaskGroup,
        btc_rpc: ServerBitcoinRpcMonitor,
    ) -> Wallet {
        Self::spawn_broadcast_unconfirmed_txs_task(btc_rpc.clone(), db.clone(), task_group);

        Wallet {
            cfg,
            btc_rpc,
            db: db.clone(),
        }
    }

    fn spawn_broadcast_unconfirmed_txs_task(
        btc_rpc: ServerBitcoinRpcMonitor,
        db: Database,
        task_group: &TaskGroup,
    ) {
        task_group.spawn_cancellable("broadcast_unconfirmed_transactions", async move {
            loop {
                let unconfirmed_txs: Vec<FederationTx> = db
                    .begin_read()
                    .await
                    .iter(&UNCONFIRMED_TX)
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect();

                for unconfirmed_tx in unconfirmed_txs {
                    btc_rpc.submit_transaction(unconfirmed_tx.tx).await;
                }

                sleep(common::sleep_duration()).await;
            }
        });
    }

    async fn process_block_count(
        &self,
        dbtx: &WriteTxRef<'_>,
        block_count_vote: u64,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        let old_consensus_block_count = self.consensus_block_count(dbtx);

        let current_vote = dbtx
            .insert(&BLOCK_COUNT_VOTE, &peer, &block_count_vote)
            .unwrap_or(0);

        ensure!(
            current_vote < block_count_vote,
            "Block count vote is redundant"
        );

        let new_consensus_block_count = self.consensus_block_count(dbtx);

        assert!(old_consensus_block_count <= new_consensus_block_count);

        // We do not sync blocks that predate the federation itself.
        if old_consensus_block_count == 0 {
            return Ok(());
        }

        // Our bitcoin backend needs to be synced for the following calls to the
        // get_block rpc to be safe for consensus.
        self.await_local_sync_to_block_count(
            new_consensus_block_count + CONFIRMATION_FINALITY_DELAY,
        )
        .await;

        for height in old_consensus_block_count..new_consensus_block_count {
            // Verify network matches (status should be available after sync)
            if let Some(status) = self.btc_rpc.status() {
                assert_eq!(status.network, self.cfg.consensus.network);
            }

            let block_hash = util::retry(
                "get_block_hash",
                util::backoff_util::background_backoff(),
                || self.btc_rpc.get_block_hash(height),
            )
            .await
            .expect("Bitcoind rpc to get_block_hash failed");

            let block = util::retry(
                "get_block",
                util::backoff_util::background_backoff(),
                || self.btc_rpc.get_block(&block_hash),
            )
            .await
            .expect("Bitcoind rpc to get_block failed");

            assert_eq!(block.block_hash(), block_hash, "Block hash mismatch");

            let pks_hash = self.cfg.consensus.bitcoin_pks.consensus_hash();

            for tx in block.txdata {
                dbtx.remove(&UNCONFIRMED_TX, &tx.compute_txid());

                // We maintain an append-only log of transaction outputs that pass
                // the probabilistic receive filter created since the federation was
                // established. This is downloaded by clients to detect pegins and
                // claim them by index.

                for (vout, tx_out) in tx.output.iter().enumerate() {
                    if is_potential_receive(&tx_out.script_pubkey, &pks_hash) {
                        let outpoint = bitcoin::OutPoint {
                            txid: tx.compute_txid(),
                            vout: u32::try_from(vout)
                                .expect("Bitcoin transaction has more than u32::MAX outputs"),
                        };

                        let index = dbtx
                            .iter(&OUTPUT)
                            .into_iter()
                            .next_back()
                            .map_or(0, |(idx, _)| idx + 1);

                        dbtx.insert(&OUTPUT, &index, &Output(outpoint, tx_out.clone()));
                    }
                }
            }
        }

        Ok(())
    }

    async fn process_signatures(
        &self,
        dbtx: &WriteTxRef<'_>,
        txid: bitcoin::Txid,
        signatures: Vec<Signature>,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        let mut unsigned = dbtx
            .get(&UNSIGNED_TX, &txid)
            .context("Unsigned transaction does not exist")?;

        let pk = self
            .cfg
            .consensus
            .bitcoin_pks
            .get(&peer)
            .expect("Failed to get public key of peer from config");

        self.verify_signatures(&unsigned, &signatures, *pk)?;

        if dbtx
            .insert(&SIGNATURES, &(txid, peer), &signatures)
            .is_some()
        {
            bail!("Already received valid signatures from this peer")
        }

        let signatures_by_peer: BTreeMap<PeerId, Vec<Signature>> = dbtx
            .range(
                &SIGNATURES,
                (txid, PeerId::from(u16::MIN))..=(txid, PeerId::from(u16::MAX)),
            )
            .into_iter()
            .map(|((_, peer), sigs)| (peer, sigs))
            .collect();

        if signatures_by_peer.len() == self.cfg.consensus.bitcoin_pks.to_num_peers().threshold() {
            dbtx.remove(&UNSIGNED_TX, &txid);

            for peer in signatures_by_peer.keys() {
                dbtx.remove(&SIGNATURES, &(txid, *peer));
            }

            self.finalize_tx(&mut unsigned, &signatures_by_peer);

            dbtx.insert(&UNCONFIRMED_TX, &txid, &unsigned);

            self.btc_rpc.submit_transaction(unsigned.tx).await;
        }

        Ok(())
    }

    async fn await_local_sync_to_block_count(&self, block_count: u64) {
        loop {
            if self
                .btc_rpc
                .status()
                .is_some_and(|status| status.block_count >= block_count)
            {
                break;
            }

            info!(target: LOG_MODULE_WALLETV2, "Waiting for local bitcoin backend to sync to block count {block_count}");

            sleep(common::sleep_duration()).await;
        }
    }

    pub fn consensus_block_count(&self, dbtx: &impl IReadDatabaseTransactionOps) -> u64 {
        let num_peers = self.cfg.consensus.bitcoin_pks.to_num_peers();

        let mut counts: Vec<u64> = dbtx
            .iter(&BLOCK_COUNT_VOTE)
            .into_iter()
            .map(|(_, v)| v)
            .collect();

        assert!(counts.len() <= num_peers.total());

        counts.sort_unstable();

        counts.reverse();

        assert!(counts.last() <= counts.first());

        // The block count we select guarantees that any threshold of correct peers can
        // increase the consensus block count and any consensus block count has been
        // confirmed by a threshold of peers.

        counts.get(num_peers.threshold() - 1).copied().unwrap_or(0)
    }

    pub fn consensus_feerate(&self, dbtx: &impl IReadDatabaseTransactionOps) -> Option<u64> {
        let num_peers = self.cfg.consensus.bitcoin_pks.to_num_peers();

        let mut rates: Vec<u64> = dbtx
            .iter(&FEE_RATE_VOTE)
            .into_iter()
            .filter_map(|(_, v)| v)
            .collect();

        assert!(rates.len() <= num_peers.total());

        rates.sort_unstable();

        assert!(rates.first() <= rates.last());

        rates.get(num_peers.threshold() - 1).copied()
    }

    pub fn consensus_fee(&self, dbtx: &WriteTxRef<'_>, tx_vbytes: u64) -> Option<Amount> {
        // The minimum feerate is a protection against a catastrophic error in the
        // feerate estimation and limits the length of the pending transaction stack.

        let pending_txs = pending_txs_unordered(dbtx);

        assert!(pending_txs.len() <= 32);

        let feerate = self
            .consensus_feerate(dbtx)?
            .max(self.cfg.consensus.feerate_base << pending_txs.len());

        let tx_fee = tx_vbytes.saturating_mul(feerate).saturating_div(1000);

        let stack_vbytes = pending_txs
            .iter()
            .map(|t| t.vbytes)
            .try_fold(tx_vbytes, u64::checked_add)
            .expect("Stack vbytes overflow with at most 32 pending txs");

        let stack_fee = stack_vbytes.saturating_mul(feerate).saturating_div(1000);

        // Deduct the fees already paid by currently pending transactions
        let stack_fee = pending_txs
            .iter()
            .map(|t| t.fee.to_sat())
            .fold(stack_fee, u64::saturating_sub);

        Some(Amount::from_sat(tx_fee.max(stack_fee)))
    }

    pub fn send_fee(&self, dbtx: &WriteTxRef<'_>) -> Option<Amount> {
        self.consensus_fee(dbtx, self.cfg.consensus.send_tx_vbytes)
    }

    pub fn receive_fee(&self, dbtx: &WriteTxRef<'_>) -> Option<Amount> {
        self.consensus_fee(dbtx, self.cfg.consensus.receive_tx_vbytes)
    }

    fn descriptor(&self, tweak: &sha256::Hash) -> Wsh<secp256k1::PublicKey> {
        descriptor(&self.cfg.consensus.bitcoin_pks, tweak)
    }

    fn sign_tx(&self, unsigned_tx: &FederationTx) -> Vec<Signature> {
        let mut sighash_cache = SighashCache::new(unsigned_tx.tx.clone());

        unsigned_tx
            .spent_tx_outs
            .iter()
            .enumerate()
            .map(|(index, utxo)| {
                let descriptor = self.descriptor(&utxo.tweak).ecdsa_sighash_script_code();

                let p2wsh_sighash = sighash_cache
                    .p2wsh_signature_hash(index, &descriptor, utxo.value, EcdsaSighashType::All)
                    .expect("Failed to compute P2WSH segwit sighash");

                let scalar = &Scalar::from_be_bytes(utxo.tweak.to_byte_array())
                    .expect("Hash is within field order");

                let sk = self
                    .cfg
                    .private
                    .bitcoin_sk
                    .add_tweak(scalar)
                    .expect("Failed to tweak bitcoin secret key");

                Secp256k1::new().sign_ecdsa(&p2wsh_sighash.into(), &sk)
            })
            .collect()
    }

    fn verify_signatures(
        &self,
        unsigned_tx: &FederationTx,
        signatures: &[Signature],
        pk: PublicKey,
    ) -> anyhow::Result<()> {
        ensure!(
            unsigned_tx.spent_tx_outs.len() == signatures.len(),
            "Incorrect number of signatures"
        );

        let mut sighash_cache = SighashCache::new(unsigned_tx.tx.clone());

        for ((index, utxo), signature) in unsigned_tx
            .spent_tx_outs
            .iter()
            .enumerate()
            .zip(signatures.iter())
        {
            let code = self.descriptor(&utxo.tweak).ecdsa_sighash_script_code();

            let p2wsh_sighash = sighash_cache
                .p2wsh_signature_hash(index, &code, utxo.value, EcdsaSighashType::All)
                .expect("Failed to compute P2WSH segwit sighash");

            let pk = tweak_public_key(&pk, &utxo.tweak);

            secp256k1::SECP256K1.verify_ecdsa(&p2wsh_sighash.into(), signature, &pk)?;
        }

        Ok(())
    }

    fn finalize_tx(
        &self,
        federation_tx: &mut FederationTx,
        signatures: &BTreeMap<PeerId, Vec<Signature>>,
    ) {
        assert_eq!(
            federation_tx.spent_tx_outs.len(),
            federation_tx.tx.input.len()
        );

        for (index, utxo) in federation_tx.spent_tx_outs.iter().enumerate() {
            let satisfier: BTreeMap<PublicKey, bitcoin::ecdsa::Signature> = signatures
                .iter()
                .map(|(peer, sigs)| {
                    assert_eq!(sigs.len(), federation_tx.tx.input.len());

                    let pk = *self
                        .cfg
                        .consensus
                        .bitcoin_pks
                        .get(peer)
                        .expect("Failed to get public key of peer from config");

                    let pk = tweak_public_key(&pk, &utxo.tweak);

                    (pk, bitcoin::ecdsa::Signature::sighash_all(sigs[index]))
                })
                .collect();

            miniscript::Descriptor::Wsh(self.descriptor(&utxo.tweak))
                .satisfy(&mut federation_tx.tx.input[index], satisfier)
                .expect("Failed to satisfy descriptor");
        }
    }

    fn tx_id(&self, dbtx: &WriteTxRef<'_>, outpoint: OutPoint) -> Option<Txid> {
        let index = dbtx.get(&TX_INFO_INDEX, &outpoint)?;

        dbtx.get(&TX_INFO, &index).map(|entry| entry.txid)
    }

    fn get_outputs(
        &self,
        dbtx: &WriteTxRef<'_>,
        start_index: u64,
        end_index: u64,
    ) -> Vec<OutputInfo> {
        let spent: BTreeSet<u64> = dbtx
            .range(&SPENT_OUTPUT, start_index..end_index)
            .into_iter()
            .map(|(idx, ())| idx)
            .collect();

        dbtx.range(&OUTPUT, start_index..end_index)
            .into_iter()
            .filter_map(|(idx, Output(_, tx_out))| {
                tx_out.script_pubkey.is_p2wsh().then(|| OutputInfo {
                    index: idx,
                    script: tx_out.script_pubkey,
                    value: tx_out.value,
                    spent: spent.contains(&idx),
                })
            })
            .collect()
    }

    fn pending_tx_chain(&self, dbtx: &WriteTxRef<'_>) -> Vec<TxInfo> {
        let n_pending = pending_txs_unordered(dbtx).len();

        let mut items: Vec<TxInfo> = dbtx.iter(&TX_INFO).into_iter().map(|(_, v)| v).collect();

        items.reverse();
        items.truncate(n_pending);
        items
    }

    fn tx_chain(&self, dbtx: &WriteTxRef<'_>) -> Vec<TxInfo> {
        dbtx.iter(&TX_INFO).into_iter().map(|(_, v)| v).collect()
    }

    fn total_txs(&self, dbtx: &WriteTxRef<'_>) -> u64 {
        dbtx.iter(&TX_INFO)
            .into_iter()
            .next_back()
            .map_or(0, |(idx, _)| idx + 1)
    }

    /// Get the network for UI display
    pub fn network_ui(&self) -> Network {
        self.cfg.consensus.network
    }

    /// Get the current federation wallet info for UI display
    pub async fn federation_wallet_ui(&self) -> Option<FederationWallet> {
        self.db.begin_read().await.get(&FEDERATION_WALLET, &())
    }

    /// Get the current consensus block count for UI display
    pub async fn consensus_block_count_ui(&self) -> u64 {
        let dbtx = self.db.begin_write().await;
        let result = self.consensus_block_count(&dbtx.as_ref());
        dbtx.commit().await;
        result
    }

    /// Get the current consensus feerate for UI display
    pub async fn consensus_feerate_ui(&self) -> Option<u64> {
        let dbtx = self.db.begin_write().await;
        let result = self.consensus_feerate(&dbtx.as_ref()).map(|f| f / 1000);
        dbtx.commit().await;
        result
    }

    /// Get the current send fee for UI display
    pub async fn send_fee_ui(&self) -> Option<Amount> {
        let dbtx = self.db.begin_write().await;
        let result = self.send_fee(&dbtx.as_ref());
        dbtx.commit().await;
        result
    }

    /// Get the current receive fee for UI display
    pub async fn receive_fee_ui(&self) -> Option<Amount> {
        let dbtx = self.db.begin_write().await;
        let result = self.receive_fee(&dbtx.as_ref());
        dbtx.commit().await;
        result
    }

    /// Get the current pending transaction info for UI display
    pub async fn pending_tx_chain_ui(&self) -> Vec<TxInfo> {
        let dbtx = self.db.begin_write().await;
        let result = self.pending_tx_chain(&dbtx.as_ref());
        dbtx.commit().await;
        result
    }

    /// Get the current transaction log for UI display
    pub async fn tx_chain_ui(&self) -> Vec<TxInfo> {
        let dbtx = self.db.begin_write().await;
        let result = self.tx_chain(&dbtx.as_ref());
        dbtx.commit().await;
        result
    }

    /// Export recovery keys for federation shutdown. Returns None if the
    /// federation wallet has not been initialized yet.
    pub async fn recovery_keys_ui(&self) -> Option<(BTreeMap<PeerId, String>, String)> {
        let wallet = self.federation_wallet_ui().await?;

        let pks = self
            .cfg
            .consensus
            .bitcoin_pks
            .iter()
            .map(|(peer, pk)| (*peer, tweak_public_key(pk, &wallet.tweak).to_string()))
            .collect();

        let tweak = &Scalar::from_be_bytes(wallet.tweak.to_byte_array())
            .expect("Hash is within field order");

        let sk = self
            .cfg
            .private
            .bitcoin_sk
            .add_tweak(tweak)
            .expect("Failed to tweak bitcoin secret key");

        let sk = bitcoin::PrivateKey::new(sk, self.cfg.consensus.network).to_wif();

        Some((pks, sk))
    }
}
