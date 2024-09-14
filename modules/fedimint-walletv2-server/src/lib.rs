#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::default_trait_access)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::too_many_lines)]

pub mod db;

use std::collections::BTreeMap;
#[cfg(not(target_family = "wasm"))]
use std::time::Duration;

use anyhow::{bail, ensure, Context};
use bitcoin::absolute::LockTime;
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::psbt::{Input, PartiallySignedTransaction};
use bitcoin::secp256k1::Secp256k1;
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::{Address, Amount, ScriptBuf, Sequence, Transaction, TxIn, TxOut};
use common::config::WalletConfigConsensus;
use common::{
    PegOutSignatures, WalletCommonInit, WalletConsensusItem, WalletCreationError, WalletInput,
    WalletModuleTypes, WalletOutput, WalletOutputOutcome, CONFIRMATION_TARGET, FEERATE_MULTIPLIER,
};
use db::{FederationUTXOKey, FeeRateIndexKey, PegOutSignaturesKey, PegOutSignaturesTxidPrefix};
use fedimint_bitcoind::{create_bitcoind, DynBitcoindRpc};
use fedimint_core::config::{
    ConfigGenModuleParams, DkgResult, ServerModuleConfig, ServerModuleConsensusConfig,
    TypedServerModuleConfig, TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{
    Database, DatabaseTransaction, DatabaseVersion, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::envs::BitcoinRpcConfig;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    api_endpoint, ApiEndpoint, ApiError, ApiVersion, CoreConsensusVersion, InputMeta,
    ModuleConsensusVersion, ModuleInit, PeerHandle, ServerModuleInit, ServerModuleInitArgs,
    SupportedModuleApiVersions, TransactionItemAmount, CORE_CONSENSUS_VERSION,
};
use fedimint_core::server::DynServerModule;
#[cfg(not(target_family = "wasm"))]
use fedimint_core::task::sleep;
use fedimint_core::task::TaskGroup;
use fedimint_core::{
    apply, async_trait_maybe_send, push_db_key_items, push_db_pair_items, util, Feerate,
    NumPeersExt, OutPoint, PeerId, ServerModule,
};
use fedimint_server::config::distributedgen::PeerHandleOps;
use fedimint_server::net::api::check_auth;
pub use fedimint_walletv2_common as common;
use fedimint_walletv2_common::config::{WalletClientConfig, WalletConfig, WalletGenParams};
use fedimint_walletv2_common::endpoint_constants::{
    BITCOIN_KIND_ENDPOINT, BITCOIN_RPC_CONFIG_ENDPOINT, BLOCK_COUNT_ENDPOINT,
    BLOCK_COUNT_LOCAL_ENDPOINT, PEG_IN_FEE_ENDPOINT, PEG_OUT_FEE_ENDPOINT,
};
use fedimint_walletv2_common::{
    descriptor, tweak_public_key, WalletInputError, WalletOutputError, MODULE_CONSENSUS_VERSION,
};
use futures::StreamExt;
use miniscript::psbt::PsbtExt;
use rand::rngs::OsRng;
use secp256k1::ecdsa::Signature;
use secp256k1::{PublicKey, Scalar};
use serde::{Deserialize, Serialize};
use strum::IntoEnumIterator;

use crate::db::{
    BlockCountVoteKey, BlockCountVotePrefix, BlockHashKey, BlockHashKeyPrefix, ClaimedUTXOKey,
    ClaimedUTXOPrefixKey, DbKeyPrefix, FeeRateVoteKey, FeeRateVotePrefix, PegOutBitcoinTransaction,
    PegOutBitcoinTransactionPrefix, PendingTransactionKey, PendingTransactionPrefixKey,
    UnsignedTransactionKey, UnsignedTransactionPrefixKey,
};

/// A PSBT that is awaiting enough signatures from the federation to becoming a
/// `PendingTransaction`
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Encodable, Decodable)]
pub struct UnsignedTransaction {
    pub transaction: Transaction,
    pub utxos: Vec<SpendableUTXO>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct SpendableUTXO {
    pub tweak: sha256::Hash,
    #[serde(with = "bitcoin::amount::serde::as_sat")]
    pub amount: Amount,
}

#[derive(Clone, Debug, Eq, PartialEq, Encodable, Decodable)]
pub struct FederationUTXO {
    pub outpoint: bitcoin::OutPoint,
    pub amount: Amount,
    pub tweak: sha256::Hash,
}

#[derive(Debug, Clone)]
pub struct WalletInit;

impl ModuleInit for WalletInit {
    type Common = WalletCommonInit;
    const DATABASE_VERSION: DatabaseVersion = DatabaseVersion(0);

    async fn dump_database(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        prefix_names: Vec<String>,
    ) -> Box<dyn Iterator<Item = (String, Box<dyn erased_serde::Serialize + Send>)> + '_> {
        let mut wallet: BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> = BTreeMap::new();
        let filtered_prefixes = DbKeyPrefix::iter().filter(|f| {
            prefix_names.is_empty() || prefix_names.contains(&f.to_string().to_lowercase())
        });
        for table in filtered_prefixes {
            match table {
                DbKeyPrefix::BlockHash => {
                    push_db_key_items!(dbtx, BlockHashKeyPrefix, BlockHashKey, wallet, "Blocks");
                }
                DbKeyPrefix::PegOutBitcoinOutPoint => {
                    push_db_pair_items!(
                        dbtx,
                        PegOutBitcoinTransactionPrefix,
                        PegOutBitcoinTransaction,
                        WalletOutputOutcome,
                        wallet,
                        "Peg Out Bitcoin Transaction"
                    );
                }
                DbKeyPrefix::PendingTransaction => {
                    push_db_pair_items!(
                        dbtx,
                        PendingTransactionPrefixKey,
                        PendingTransactionKey,
                        Transaction,
                        wallet,
                        "Pending Transactions"
                    );
                }
                DbKeyPrefix::UnsignedTransaction => {
                    push_db_pair_items!(
                        dbtx,
                        UnsignedTransactionPrefixKey,
                        UnsignedTransactionKey,
                        UnsignedTransaction,
                        wallet,
                        "Unsigned Transactions"
                    );
                }
                DbKeyPrefix::ClaimedUtxo => {
                    push_db_pair_items!(
                        dbtx,
                        ClaimedUTXOPrefixKey,
                        ClaimedUTXOKey,
                        (),
                        wallet,
                        "Claimed UTXOs"
                    );
                }
                DbKeyPrefix::BlockCountVote => {
                    push_db_pair_items!(
                        dbtx,
                        BlockCountVotePrefix,
                        BlockCountVoteKey,
                        u64,
                        wallet,
                        "Block Count Votes"
                    );
                }

                DbKeyPrefix::FeeRateVote => {
                    push_db_pair_items!(
                        dbtx,
                        FeeRateVotePrefix,
                        FeeRateVoteKey,
                        Option<Feerate>,
                        wallet,
                        "Fee Rate Votes"
                    );
                }
                DbKeyPrefix::FeeRateCounter => todo!(),
                DbKeyPrefix::PegOutSignatures => todo!(),
                DbKeyPrefix::FederationUtxo => todo!(),
            }
        }

        Box::new(wallet.into_iter())
    }
}

#[apply(async_trait_maybe_send!)]
impl ServerModuleInit for WalletInit {
    type Params = WalletGenParams;

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    fn supported_api_versions(&self) -> SupportedModuleApiVersions {
        SupportedModuleApiVersions::from_raw(
            (CORE_CONSENSUS_VERSION.major, CORE_CONSENSUS_VERSION.minor),
            (
                MODULE_CONSENSUS_VERSION.major,
                MODULE_CONSENSUS_VERSION.minor,
            ),
            &[(0, 1)],
        )
    }

    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<DynServerModule> {
        Ok(
            Wallet::new(args.cfg().to_typed()?, args.db(), args.task_group())
                .await?
                .into(),
        )
    }

    fn trusted_dealer_gen(
        &self,
        peers: &[PeerId],
        params: &ConfigGenModuleParams,
    ) -> BTreeMap<PeerId, ServerModuleConfig> {
        let params = self.parse_params(params).unwrap();
        let secp = secp256k1::Secp256k1::new();

        let btc_pegin_keys = peers
            .iter()
            .map(|&id| (id, secp.generate_keypair(&mut OsRng)))
            .collect::<Vec<_>>();

        let wallet_cfg: BTreeMap<PeerId, WalletConfig> = btc_pegin_keys
            .iter()
            .map(|(id, (sk, _))| {
                let cfg = WalletConfig::new(
                    params.local.bitcoin_rpc.clone(),
                    *sk,
                    btc_pegin_keys
                        .iter()
                        .map(|(peer_id, (_, pk))| (*peer_id, *pk))
                        .collect(),
                    params.consensus.fee_consensus.clone(),
                    params.consensus.network,
                );
                (*id, cfg)
            })
            .collect();

        wallet_cfg
            .into_iter()
            .map(|(k, v)| (k, v.to_erased()))
            .collect()
    }

    async fn distributed_gen(
        &self,
        peers: &PeerHandle,
        params: &ConfigGenModuleParams,
    ) -> DkgResult<ServerModuleConfig> {
        let params = self.parse_params(params).unwrap();
        let (bitcoin_sk, bitcoin_pk) = secp256k1::generate_keypair(&mut OsRng);

        let bitcoin_pks: BTreeMap<PeerId, PublicKey> = peers
            .exchange_pubkeys("wallet".to_string(), bitcoin_pk)
            .await?
            .into_iter()
            .collect();

        let wallet_cfg = WalletConfig::new(
            params.local.bitcoin_rpc.clone(),
            bitcoin_sk,
            bitcoin_pks,
            params.consensus.fee_consensus,
            params.consensus.network,
        );

        Ok(wallet_cfg.to_erased())
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
            fee_consensus: config.fee_consensus,
            network: config.network,
            finality_delay: config.finality_delay,
        })
    }
}

#[apply(async_trait_maybe_send!)]
impl ServerModule for Wallet {
    type Common = WalletModuleTypes;
    type Init = WalletInit;

    async fn consensus_proposal<'a>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'_>,
    ) -> Vec<WalletConsensusItem> {
        let mut items = dbtx
            .find_by_prefix(&UnsignedTransactionPrefixKey)
            .await
            .map(|(key, unsigned_transaction)| {
                let signatures = self.sign_transaction(
                    &unsigned_transaction.utxos,
                    &unsigned_transaction.transaction,
                );

                // Verify our signatures against our private key
                assert!(
                    self.verify_signatures(
                        &unsigned_transaction.utxos,
                        &unsigned_transaction.transaction,
                        &signatures,
                        &self.cfg.private.bitcoin_sk.public_key(secp256k1::SECP256K1)
                    )
                    .is_ok(),
                    "Our signatures failed verification against our private key"
                );

                WalletConsensusItem::PegOutSignature(PegOutSignatures {
                    txid: key.0,
                    signatures,
                })
            })
            .collect::<Vec<WalletConsensusItem>>()
            .await;

        if let Ok(block_count) = self.btc_rpc.get_block_count().await {
            items.push(WalletConsensusItem::BlockCount(
                block_count.saturating_sub(self.cfg.consensus.finality_delay),
            ));
        }

        // If we cannot fetch a up to date feerate from our bitcoin backend we need to
        // disable our last vote as an outdated consensus feerate can get our
        // transactions stuck.
        if let Ok(Some(fee_rate)) = self.btc_rpc.get_fee_rate(CONFIRMATION_TARGET).await {
            items.push(WalletConsensusItem::Feerate(Some(Feerate {
                sats_per_kvb: fee_rate.sats_per_kvb * FEERATE_MULTIPLIER,
            })));
        } else {
            items.push(WalletConsensusItem::Feerate(None));
        }

        items
    }

    async fn process_consensus_item<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        consensus_item: WalletConsensusItem,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        match consensus_item {
            WalletConsensusItem::BlockCount(block_count_vote) => {
                let current_vote = dbtx.get_value(&BlockCountVoteKey(peer)).await.unwrap_or(0);

                ensure!(
                    block_count_vote > current_vote,
                    "Block count vote is redundant"
                );

                let old_consensus_block_count = self.consensus_block_count(dbtx).await;

                dbtx.insert_entry(&BlockCountVoteKey(peer), &block_count_vote)
                    .await;

                let new_consensus_block_count = self.consensus_block_count(dbtx).await;

                assert!(old_consensus_block_count <= new_consensus_block_count);

                // We do not sync blocks that predate the federation itself
                if old_consensus_block_count != 0 {
                    for height in old_consensus_block_count..new_consensus_block_count {
                        let block_hash = util::retry(
                            "get_block_hash",
                            util::backoff_util::background_backoff(),
                            || self.btc_rpc.get_block_hash(u64::from(height)),
                        )
                        .await
                        .expect("bitcoind rpc to get block hash");

                        dbtx.insert_new_entry(&BlockHashKey(block_hash), &()).await;
                    }
                }
            }
            WalletConsensusItem::Feerate(feerate) => {
                let old_consensus_feerate = self.consensus_fee_rate(dbtx).await;

                if Some(feerate) == dbtx.insert_entry(&FeeRateVoteKey(peer), &feerate).await {
                    bail!("Fee rate vote is redundant");
                }

                let new_consensus_feerate = self.consensus_fee_rate(dbtx).await;

                if new_consensus_feerate != old_consensus_feerate {
                    let index = dbtx.remove_entry(&FeeRateIndexKey).await.unwrap_or(0) + 1;

                    dbtx.insert_new_entry(&FeeRateIndexKey, &index).await
                }
            }
            WalletConsensusItem::PegOutSignature(peg_out_signatures) => {
                let unsigned = dbtx
                    .get_value(&UnsignedTransactionKey(peg_out_signatures.txid))
                    .await
                    .context("Unsigned transaction does not exist")?;

                self.verify_signatures(
                    &unsigned.utxos,
                    &unsigned.transaction,
                    &peg_out_signatures.signatures,
                    &self.get_expect_peer_bitcoin_pk(peer),
                )?;

                if dbtx
                    .insert_entry(
                        &PegOutSignaturesKey(peg_out_signatures.txid, peer),
                        &peg_out_signatures.signatures,
                    )
                    .await
                    .is_some()
                {
                    bail!("Already received valid signature from this peer")
                }

                let signatures = dbtx
                    .find_by_prefix(&PegOutSignaturesTxidPrefix(peg_out_signatures.txid))
                    .await
                    .map(|(key, signatures)| (self.get_expect_peer_bitcoin_pk(key.1), signatures))
                    .collect::<BTreeMap<PublicKey, Vec<Signature>>>()
                    .await;

                let num_peers = self.cfg.consensus.bitcoin_pks.to_num_peers();

                if signatures.len() == num_peers.threshold() {
                    let transaction = self.finalize_transaction(
                        &unsigned.utxos,
                        unsigned.transaction,
                        signatures,
                    );

                    dbtx.insert_new_entry(
                        &PendingTransactionKey(peg_out_signatures.txid),
                        &transaction,
                    )
                    .await;

                    dbtx.remove_entry(&UnsignedTransactionKey(peg_out_signatures.txid))
                        .await;
                }
            }
            WalletConsensusItem::Default { variant, .. } => {
                bail!("Received wallet consensus item with unknown variant {variant}");
            }
        }

        Ok(())
    }

    async fn process_input<'a, 'b, 'c>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'c>,
        input: &'b WalletInput,
    ) -> Result<InputMeta, WalletInputError> {
        let input = input.ensure_v0_ref()?;

        input.proof.verify()?;

        if dbtx
            .get_value(&ClaimedUTXOKey(input.proof.outpoint()))
            .await
            .is_some()
        {
            return Err(WalletInputError::PegInAlreadyClaimed);
        }

        if dbtx
            .get_value(&BlockHashKey(input.proof.block_hash()))
            .await
            .is_none()
        {
            return Err(WalletInputError::UnknownPegInProofBlock(
                input.proof.block_hash(),
            ));
        }

        if input.proof.script()
            != descriptor(
                &self.cfg.consensus.bitcoin_pks,
                &input.tweak.consensus_hash(),
            )
            .script_pubkey()
        {
            return Err(WalletInputError::WrongOutputScript);
        }

        // This check ensures that once a Fedimint transaction becomes invalid due to a
        // change in feerate it stays invalid. Otherwise there exists a race condition
        // where a client might consider a actually accepted transaction as rejected.
        if input.fee_index != dbtx.get_value(&FeeRateIndexKey).await.unwrap_or(0) {
            return Err(WalletInputError::IncorrectFeeRateIndex);
        }

        let pegin_value = match dbtx.remove_entry(&FederationUTXOKey).await {
            Some(federation_utxo) => {
                let (transaction, pegin_value, change, utxos) = self
                    .create_pegin_tx(
                        input.proof.amount(),
                        input.proof.outpoint(),
                        input.tweak.consensus_hash(),
                        federation_utxo.clone(),
                        self.consensus_fee_rate(dbtx)
                            .await
                            .ok_or(WalletInputError::NoFeerateAvailable)?,
                    )
                    .ok_or(WalletInputError::ArithmeticOverflow)?;

                assert_eq!(
                    federation_utxo
                        .amount
                        .checked_add(pegin_value)
                        .ok_or(WalletInputError::ArithmeticOverflow)?,
                    change
                );

                self.process_transaction(
                    dbtx,
                    transaction,
                    change,
                    federation_utxo.consensus_hash(),
                    utxos,
                )
                .await;

                pegin_value
            }
            None => {
                if let Some(federation_utxo) = dbtx
                    .insert_entry(
                        &FederationUTXOKey,
                        &FederationUTXO {
                            outpoint: input.proof.outpoint(),
                            amount: input.proof.amount(),
                            tweak: input.tweak.consensus_hash(),
                        },
                    )
                    .await
                {
                    panic!("Attempt to overwrite federation utxo {:?}", federation_utxo)
                }

                input.proof.amount()
            }
        };

        let amount = fedimint_core::Amount::from_msats(
            pegin_value
                .to_sat()
                .checked_mul(1000)
                .ok_or(WalletInputError::ArithmeticOverflow)?,
        );

        Ok(InputMeta {
            amount: TransactionItemAmount {
                amount,
                fee: self.cfg.consensus.fee_consensus.fee(amount),
            },
            pub_key: input.tweak,
        })
    }

    async fn process_output<'a, 'b>(
        &'a self,
        dbtx: &mut DatabaseTransaction<'b>,
        output: &'a WalletOutput,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmount, WalletOutputError> {
        let output = output.ensure_v0_ref()?;

        if !output
            .address
            .is_valid_for_network(self.cfg.consensus.network)
        {
            return Err(WalletOutputError::WrongNetwork(
                self.cfg.consensus.network,
                output.address.network,
            ));
        }

        // Validate the tx amount is over the dust limit
        if output.amount
            < output
                .address
                .clone()
                .assume_checked()
                .script_pubkey()
                .dust_value()
        {
            return Err(WalletOutputError::PegOutUnderDustLimit);
        }

        let federation_utxo = dbtx
            .remove_entry(&FederationUTXOKey)
            .await
            .ok_or(WalletOutputError::NoFederationUTXO)?;

        // This check ensures that once a Fedimint transaction becomes invalid due to a
        // change in feerate it stays invalid. Otherwise there exists a race condition
        // where a client might consider a actually accepted transaction as rejected.
        if output.fee_index != dbtx.get_value(&FeeRateIndexKey).await.unwrap_or(0) {
            return Err(WalletOutputError::IncorrectFeeRateIndex);
        }

        let (transaction, pegout_value, change, utxos) = self
            .create_pegout_tx(
                output.amount,
                output.address.clone().assume_checked().script_pubkey(),
                federation_utxo.clone(),
                self.consensus_fee_rate(dbtx)
                    .await
                    .ok_or(WalletOutputError::NoFeerateAvailable)?,
            )
            .ok_or(WalletOutputError::ArithmeticOverflow)?;

        assert_eq!(
            pegout_value
                .checked_add(change)
                .ok_or(WalletOutputError::ArithmeticOverflow)?,
            federation_utxo.amount
        );

        dbtx.insert_new_entry(
            &PegOutBitcoinTransaction(out_point),
            &WalletOutputOutcome::new_v0(transaction.txid()),
        )
        .await;

        self.process_transaction(
            dbtx,
            transaction,
            change,
            federation_utxo.consensus_hash(),
            utxos,
        )
        .await;

        let amount = fedimint_core::Amount::from_msats(
            pegout_value
                .to_sat()
                .checked_mul(1000)
                .ok_or(WalletOutputError::ArithmeticOverflow)?,
        );

        Ok(TransactionItemAmount {
            amount,
            fee: self.cfg.consensus.fee_consensus.fee(amount),
        })
    }

    async fn output_status(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        out_point: OutPoint,
    ) -> Option<WalletOutputOutcome> {
        dbtx.get_value(&PegOutBitcoinTransaction(out_point)).await
    }

    async fn audit(
        &self,
        _dbtx: &mut DatabaseTransaction<'_>,
        _audit: &mut Audit,
        _module_instance_id: ModuleInstanceId,
    ) {
        todo!()
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            api_endpoint! {
                BLOCK_COUNT_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, context, _params: ()| -> u64 {
                    Ok(module.consensus_block_count(&mut context.dbtx().into_nc()).await)
                }
            },
            api_endpoint! {
                BLOCK_COUNT_LOCAL_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, _context, _params: ()| -> u64 {
                    module.btc_rpc.get_block_count().await.map_err(|e| ApiError::server_error(e.to_string()))
                }
            },
            api_endpoint! {
                PEG_OUT_FEE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, context, address: Address<NetworkUnchecked>| -> Option<(u64, fedimint_core::Amount)> {
                    let mut dbtx = context.dbtx().into_nc();

                    let index = dbtx.get_value(&FeeRateIndexKey).await.unwrap_or(0);

                    let fee = module.consensus_fee_rate(&mut dbtx).await.map(|feerate| {
                        let fee = module
                            .pegout_tx_fee(&address.assume_checked().script_pubkey(), &feerate)
                            .into();

                        (index, fee)
                    });

                    Ok(fee)
                }
            },
            api_endpoint! {
                PEG_IN_FEE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Wallet, context, _params: ()| -> Option<(u64, fedimint_core::Amount)> {
                    let mut dbtx = context.dbtx().into_nc();

                    let index = dbtx.get_value(&FeeRateIndexKey).await.unwrap_or(0);

                    match dbtx.get_value(&FederationUTXOKey).await {
                        Some(..) => {
                            let fee = module
                                .consensus_fee_rate(&mut dbtx)
                                .await
                                .map(|feerate| (index, module.pegin_tx_fee(&feerate).into()));

                            Ok(fee)
                        },
                        None => Ok(Some((index, Amount::ZERO.into())))
                    }
                }
            },
            api_endpoint! {
                BITCOIN_KIND_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Wallet, _context, _params: ()| -> String {
                    Ok(module.btc_rpc.get_bitcoin_rpc_config().kind)
                }
            },
            api_endpoint! {
                BITCOIN_RPC_CONFIG_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Wallet, context, _params: ()| -> BitcoinRpcConfig {
                    check_auth(context)?;
                    let config = module.btc_rpc.get_bitcoin_rpc_config();

                    // we need to remove auth, otherwise we'll send over the wire
                    let without_auth = config.url.clone().without_auth().map_err(|_| {
                        ApiError::server_error("Unable to remove auth from bitcoin config URL".to_string())
                    })?;

                    Ok(BitcoinRpcConfig {
                        url: without_auth,
                        ..config
                    })
                }
            },
        ]
    }
}

#[derive(Debug)]
pub struct Wallet {
    cfg: WalletConfig,
    btc_rpc: DynBitcoindRpc,
}

impl Wallet {
    pub async fn new(
        cfg: WalletConfig,
        db: &Database,
        task_group: &TaskGroup,
    ) -> anyhow::Result<Wallet> {
        Ok(Self::new_with_bitcoind(
            create_bitcoind(&cfg.local.bitcoin_rpc, task_group.make_handle())?,
            cfg,
            db,
            task_group,
        )
        .await?)
    }

    pub async fn new_with_bitcoind(
        btc_rpc: DynBitcoindRpc,
        cfg: WalletConfig,
        db: &Database,
        task_group: &TaskGroup,
    ) -> Result<Wallet, WalletCreationError> {
        let bitcoind_net = btc_rpc
            .get_network()
            .await
            .map_err(|e| WalletCreationError::RpcError(e.to_string()))?;

        if bitcoind_net != cfg.consensus.network {
            return Err(WalletCreationError::WrongNetwork(
                cfg.consensus.network,
                bitcoind_net,
            ));
        }

        Self::spawn_broadcast_pending_task(task_group, btc_rpc.clone(), db.clone());

        Ok(Wallet { cfg, btc_rpc })
    }

    fn get_expect_peer_bitcoin_pk(&self, peer: PeerId) -> PublicKey {
        self.cfg
            .consensus
            .bitcoin_pks
            .get(&peer)
            .expect("Failed to get peers pegin key from config")
            .clone()
    }

    async fn process_transaction(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        transaction: Transaction,
        amount: bitcoin::Amount,
        tweak: sha256::Hash,
        utxos: Vec<SpendableUTXO>,
    ) {
        if let Some(federation_utxo) = dbtx
            .insert_entry(
                &FederationUTXOKey,
                &FederationUTXO {
                    outpoint: bitcoin::OutPoint {
                        txid: transaction.txid(),
                        vout: 0,
                    },
                    amount,
                    tweak,
                },
            )
            .await
        {
            panic!("Attempt to overwrite federation utxo {:?}", federation_utxo)
        }

        dbtx.insert_new_entry(
            &UnsignedTransactionKey(transaction.txid()),
            &UnsignedTransaction { transaction, utxos },
        )
        .await;
    }

    pub async fn consensus_block_count(&self, dbtx: &mut DatabaseTransaction<'_>) -> u64 {
        let num_peers = self.cfg.consensus.bitcoin_pks.to_num_peers();

        let mut counts = dbtx
            .find_by_prefix(&BlockCountVotePrefix)
            .await
            .map(|entry| entry.1)
            .collect::<Vec<u64>>()
            .await;

        while counts.len() < num_peers.total() {
            counts.push(0);
        }

        assert_eq!(counts.len(), num_peers.total());

        counts.sort_unstable();

        assert!(counts.first() <= counts.last());

        // The block count we select guarantees that any threshold of correct peers can
        // increase the consensus block count and any consensus block count has been
        // confirmed by a threshold of peers.

        counts[num_peers.max_evil()]
    }

    pub async fn consensus_fee_rate(&self, dbtx: &mut DatabaseTransaction<'_>) -> Option<Feerate> {
        let num_peers = self.cfg.consensus.bitcoin_pks.to_num_peers();

        let mut rates = dbtx
            .find_by_prefix(&FeeRateVotePrefix)
            .await
            .filter_map(|entry| async move { entry.1 })
            .collect::<Vec<Feerate>>()
            .await;

        if rates.len() < num_peers.threshold() {
            return None;
        }

        rates.sort_unstable();

        assert!(rates.first() <= rates.last());

        // The block count we select guarantees that any threshold of correct peers can
        // change the consensus feerate and any consensus feerate has been confirmed by
        // a threshold of peers.

        Some(rates[num_peers.threshold() - 1])
    }

    fn spawn_broadcast_pending_task(
        task_group: &TaskGroup,
        bitcoind: DynBitcoindRpc,
        db: Database,
    ) {
        task_group.spawn("broadcast pending", |task_handle| async move {
            while !task_handle.is_shutting_down() {
                let pending_transactions = db
                    .begin_transaction_nc()
                    .await
                    .find_by_prefix(&PendingTransactionPrefixKey)
                    .await
                    .map(|entry| entry.1)
                    .collect::<Vec<Transaction>>()
                    .await;

                for transaction in pending_transactions {
                    bitcoind.submit_transaction(transaction).await;
                }

                sleep(Duration::from_secs(10)).await;
            }
        });
    }

    fn pegin_tx_fee(&self, fee_rate: &Feerate) -> Amount {
        calculate_fee(fee_rate, self.cfg.consensus.pegin_weight)
    }

    fn create_pegin_tx(
        &self,
        pegin_amount: Amount,
        pegin_outpoint: bitcoin::OutPoint,
        pegin_tweak: sha256::Hash,
        federation_utxo: FederationUTXO,
        fee_rate: Feerate,
    ) -> Option<(Transaction, Amount, Amount, Vec<SpendableUTXO>)> {
        let total_fee = self.pegin_tx_fee(&fee_rate);

        let pegin_value = pegin_amount.checked_sub(total_fee)?;

        // The change is non-zero as it contains the fees taken by the federation
        let change = federation_utxo.amount.checked_add(pegin_value)?;

        let transaction = Transaction {
            version: 2,
            lock_time: LockTime::ZERO,
            input: vec![
                TxIn {
                    previous_output: federation_utxo.outpoint,
                    script_sig: Default::default(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: bitcoin::Witness::new(),
                },
                TxIn {
                    previous_output: pegin_outpoint,
                    script_sig: Default::default(),
                    sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                    witness: bitcoin::Witness::new(),
                },
            ],
            output: vec![TxOut {
                value: change.to_sat(),
                script_pubkey: descriptor(
                    &self.cfg.consensus.bitcoin_pks,
                    &federation_utxo.consensus_hash(),
                )
                .script_pubkey(),
            }],
        };

        let utxos = vec![
            SpendableUTXO {
                amount: federation_utxo.amount,
                tweak: federation_utxo.tweak,
            },
            SpendableUTXO {
                amount: pegin_amount,
                tweak: pegin_tweak,
            },
        ];

        Some((transaction, pegin_value, change, utxos))
    }

    fn pegout_tx_fee(&self, pegout_script: &ScriptBuf, fee_rate: &Feerate) -> Amount {
        calculate_fee(
            fee_rate,
            4 * pegout_script.len() as u64 + self.cfg.consensus.pegout_weight,
        )
    }

    fn create_pegout_tx(
        &self,
        pegout_amount: Amount,
        pegout_script: ScriptBuf,
        federation_utxo: FederationUTXO,
        fee_rate: Feerate,
    ) -> Option<(Transaction, Amount, Amount, Vec<SpendableUTXO>)> {
        let total_fee = self.pegout_tx_fee(&pegout_script, &fee_rate);

        let pegout_value = pegout_amount.checked_add(total_fee)?;

        // The change is non-zero as it contains the fees taken by the federation
        let change = federation_utxo.amount.checked_sub(pegout_value)?;

        let transaction = Transaction {
            version: 2,
            lock_time: LockTime::ZERO,
            input: vec![TxIn {
                previous_output: federation_utxo.outpoint,
                script_sig: Default::default(),
                sequence: Sequence::ENABLE_RBF_NO_LOCKTIME,
                witness: bitcoin::Witness::new(),
            }],
            output: vec![
                TxOut {
                    value: change.to_sat(),
                    script_pubkey: descriptor(
                        &self.cfg.consensus.bitcoin_pks,
                        &federation_utxo.consensus_hash(),
                    )
                    .script_pubkey(),
                },
                TxOut {
                    value: pegout_amount.to_sat(),
                    script_pubkey: pegout_script.clone(),
                },
            ],
        };

        let utxos = vec![SpendableUTXO {
            amount: federation_utxo.amount,
            tweak: federation_utxo.tweak,
        }];

        Some((transaction, pegout_value, change, utxos))
    }

    fn sign_transaction(
        &self,
        utxos: &Vec<SpendableUTXO>,
        transaction: &Transaction,
    ) -> Vec<Signature> {
        let mut tx_hasher = SighashCache::new(transaction);

        utxos
            .iter()
            .enumerate()
            .map(|(idx, utxo)| {
                let witness_script = descriptor(&self.cfg.consensus.bitcoin_pks, &utxo.tweak)
                    .script_code()
                    .expect("Failed to tweak descriptor");

                let tx_hash = tx_hasher
                    .segwit_signature_hash(
                        idx,
                        witness_script.as_script(),
                        utxo.amount.to_sat(),
                        EcdsaSighashType::All,
                    )
                    .expect("Failed to create segwit sighash");

                let tweaked_secret_key = self
                    .cfg
                    .private
                    .bitcoin_sk
                    .add_tweak(
                        &Scalar::from_be_bytes(utxo.tweak.to_byte_array())
                            .expect("Hash is within field order"),
                    )
                    .expect("Failed to tweak bitcoin public key");

                Secp256k1::new().sign_ecdsa(&tx_hash.into(), &tweaked_secret_key)
            })
            .collect()
    }

    fn verify_signatures(
        &self,
        utxos: &Vec<SpendableUTXO>,
        transaction: &Transaction,
        signatures: &Vec<Signature>,
        peer_key: &PublicKey,
    ) -> anyhow::Result<()> {
        let mut tx_hasher = SighashCache::new(transaction);

        if utxos.len() != signatures.len() {
            bail!("Incorrect number of signatures");
        }

        for (idx, (utxo, signature)) in utxos.iter().zip(signatures.iter()).enumerate() {
            let witness_script = descriptor(&self.cfg.consensus.bitcoin_pks, &utxo.tweak)
                .script_code()
                .expect("Failed to tweak descriptor");

            let tx_hash = tx_hasher
                .segwit_signature_hash(
                    idx,
                    witness_script.as_script(),
                    utxo.amount.to_sat(),
                    EcdsaSighashType::All,
                )
                .expect("Failed to create segwit sighash");

            secp256k1::SECP256K1.verify_ecdsa(
                &tx_hash.into(),
                signature,
                &tweak_public_key(peer_key, &utxo.tweak),
            )?;
        }

        Ok(())
    }

    fn finalize_transaction(
        &self,
        utxos: &Vec<SpendableUTXO>,
        unsigned_tx: Transaction,
        signatures: BTreeMap<PublicKey, Vec<Signature>>,
    ) -> Transaction {
        PartiallySignedTransaction {
            unsigned_tx,
            version: 0,
            xpub: Default::default(),
            proprietary: Default::default(),
            unknown: Default::default(),
            inputs: utxos
                .iter()
                .enumerate()
                .map(|(index, utxo)| Input {
                    witness_utxo: Some(TxOut {
                        value: utxo.amount.to_sat(),
                        script_pubkey: descriptor(&self.cfg.consensus.bitcoin_pks, &utxo.tweak)
                            .script_pubkey(),
                    }),
                    witness_script: Some(
                        descriptor(&self.cfg.consensus.bitcoin_pks, &utxo.tweak)
                            .script_code()
                            .expect("Failed to tweak descriptor"),
                    ),
                    partial_sigs: signatures
                        .iter()
                        .map(|(peer_key, signatures)| {
                            (
                                bitcoin::PublicKey::new(tweak_public_key(&peer_key, &utxo.tweak)),
                                bitcoin::ecdsa::Signature::sighash_all(signatures[index]),
                            )
                        })
                        .collect(),
                    ..Default::default()
                })
                .collect(),
            outputs: vec![Default::default()],
        }
        .finalize(secp256k1::SECP256K1)
        .expect("Failed to finalize PSBT")
        .extract_tx()
    }
}

pub fn calculate_fee(fee_rate: &Feerate, weight: u64) -> Amount {
    Amount::from_sat(weight_to_vbytes(weight) * fee_rate.sats_per_kvb / 1000)
}

const WITNESS_SCALE_FACTOR: u64 = bitcoin::constants::WITNESS_SCALE_FACTOR as u64;

/// Converts weight to virtual bytes, defined in [BIP-141] as weight / 4
/// (rounded up to the next integer).
///
/// [BIP-141]: https://github.com/bitcoin/bips/blob/master/bip-0141.mediawiki#transaction-size-calculations
pub fn weight_to_vbytes(weight: u64) -> u64 {
    (weight + WITNESS_SCALE_FACTOR - 1) / WITNESS_SCALE_FACTOR
}
