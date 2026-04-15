#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::similar_names)]

mod db;

use std::collections::BTreeMap;

use anyhow::{bail, ensure};
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::DatabaseVersion;
use fedimint_core::db::v2::{
    IReadDatabaseTransactionOps, IReadDatabaseTransactionOpsTyped as _,
    IWriteDatabaseTransactionOpsTyped as _,
};
use fedimint_core::encoding::Encodable;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    ApiEndpoint, ApiError, ApiVersion, CoreConsensusVersion, InputMeta, ModuleConsensusVersion,
    ModuleInit, TransactionItemAmounts, api_endpoint,
};
use fedimint_core::{Amount, InPoint, OutPoint, PeerId, apply, async_trait_maybe_send};
use fedimint_mintv2_common::config::{
    MintClientConfig, MintConfig, MintConfigConsensus, MintConfigPrivate, consensus_denominations,
};
use fedimint_mintv2_common::endpoint_constants::{
    RECOVERY_COUNT_ENDPOINT, RECOVERY_SLICE_ENDPOINT, RECOVERY_SLICE_HASH_ENDPOINT,
    SIGNATURE_SHARES_ENDPOINT, SIGNATURE_SHARES_RECOVERY_ENDPOINT,
};
use fedimint_mintv2_common::{
    Denomination, MODULE_CONSENSUS_VERSION, MintCommonInit, MintConsensusItem, MintInput,
    MintInputError, MintModuleTypes, MintOutput, MintOutputError, RecoveryItem, verify_note,
};
use fedimint_redb::v2::{Database, ReadTxRef, WriteTxRef};
use fedimint_server_core::config::{PeerHandleOps, eval_poly_g2};
use fedimint_server_core::migration::ServerModuleDbMigrationFn;
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
use tbs::{AggregatePublicKey, BlindedSignatureShare, PublicKeyShare, derive_pk_share};
use threshold_crypto::group::Curve;

use crate::db::{
    BLINDED_SIGNATURE_SHARE, BLINDED_SIGNATURE_SHARE_RECOVERY, ISSUANCE_COUNTER, NOTE_NONCE,
    RECOVERY_ITEM,
};

#[derive(Debug, Clone)]
pub struct MintInit;

impl ModuleInit for MintInit {
    type Common = MintCommonInit;
}

#[apply(async_trait_maybe_send!)]
impl ServerModuleInit for MintInit {
    type Module = Mint;

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        args.cfg().to_typed().map(|cfg| Mint {
            cfg,
            db: args.db().clone(),
        })
    }

    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        let mut tbs_sks = BTreeMap::new();
        let mut tbs_agg_pks = BTreeMap::new();
        let mut tbs_pks = BTreeMap::new();

        for denomination in consensus_denominations() {
            let (poly, sk) = peers.run_dkg_g2().await?;

            tbs_sks.insert(denomination, tbs::SecretKeyShare(sk));

            tbs_agg_pks.insert(denomination, AggregatePublicKey(poly[0].to_affine()));

            let pks = peers
                .num_peers()
                .peer_ids()
                .map(|peer| (peer, PublicKeyShare(eval_poly_g2(&poly, &peer))))
                .collect();

            tbs_pks.insert(denomination, pks);
        }

        let cfg = MintConfig {
            private: MintConfigPrivate { tbs_sks },
            consensus: MintConfigConsensus {
                tbs_agg_pks,
                tbs_pks,
                input_fee: Amount::from_msats(100),
                output_fee: Amount::from_msats(100),
            },
        };

        Ok(cfg.to_erased())
    }

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()> {
        let config = config.to_typed::<MintConfig>()?;

        for denomination in consensus_denominations() {
            let pk = derive_pk_share(&config.private.tbs_sks[&denomination]);

            ensure!(
                pk == config.consensus.tbs_pks[&denomination][identity],
                "Mint private key doesn't match pubkey share"
            );
        }

        Ok(())
    }

    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<MintClientConfig> {
        let config = MintConfigConsensus::from_erased(config)?;

        Ok(MintClientConfig {
            tbs_agg_pks: config.tbs_agg_pks,
            tbs_pks: config.tbs_pks.clone(),
            input_fee: config.input_fee,
            output_fee: config.output_fee,
        })
    }

    fn get_database_migrations(
        &self,
    ) -> BTreeMap<DatabaseVersion, ServerModuleDbMigrationFn<Mint>> {
        BTreeMap::new()
    }
}

#[derive(Debug)]
pub struct Mint {
    cfg: MintConfig,
    db: Database,
}

impl Mint {
    pub async fn note_distribution_ui(&self) -> BTreeMap<Denomination, u64> {
        self.db
            .begin_read()
            .await
            .iter(&ISSUANCE_COUNTER)
            .into_iter()
            .filter(|(_, count)| *count > 0)
            .collect()
    }
}

#[apply(async_trait_maybe_send!)]
impl ServerModule for Mint {
    type Common = MintModuleTypes;
    type Init = MintInit;

    async fn consensus_proposal(&self, _dbtx: &ReadTxRef<'_>) -> Vec<MintConsensusItem> {
        Vec::new()
    }

    async fn process_consensus_item(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _consensus_item: MintConsensusItem,
        _peer_id: PeerId,
    ) -> anyhow::Result<()> {
        bail!("Mint does not process consensus items");
    }

    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &MintInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, MintInputError> {
        let input = input.ensure_v0_ref()?;

        let pk = self
            .cfg
            .consensus
            .tbs_agg_pks
            .get(&input.note.denomination)
            .ok_or(MintInputError::InvalidDenomination)?;

        if !verify_note(input.note, *pk) {
            return Err(MintInputError::InvalidSignature);
        }

        if dbtx.insert(&NOTE_NONCE, &input.note.nonce, &()).is_some() {
            return Err(MintInputError::SpentCoin);
        }

        let new_count = dbtx
            .remove(&ISSUANCE_COUNTER, &input.note.denomination)
            .unwrap_or(0)
            .checked_sub(1)
            .expect("Failed to decrement issuance counter");

        dbtx.insert(&ISSUANCE_COUNTER, &input.note.denomination, &new_count);

        let next_index = get_recovery_count(dbtx);

        dbtx.insert(
            &RECOVERY_ITEM,
            &next_index,
            &RecoveryItem::Input {
                nonce_hash: input.note.nonce.consensus_hash(),
            },
        );

        let amount = input.note.amount();

        Ok(InputMeta {
            amount: TransactionItemAmounts {
                amount,
                fee: self.cfg.consensus.input_fee,
            },
            pub_key: input.note.nonce,
        })
    }

    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &MintOutput,
        outpoint: OutPoint,
    ) -> Result<TransactionItemAmounts, MintOutputError> {
        let output = output.ensure_v0_ref()?;

        let signature = self
            .cfg
            .private
            .tbs_sks
            .get(&output.denomination)
            .map(|key| tbs::sign_message(output.nonce, *key))
            .ok_or(MintOutputError::InvalidDenomination)?;

        dbtx.insert(&BLINDED_SIGNATURE_SHARE, &outpoint, &signature);

        dbtx.insert(&BLINDED_SIGNATURE_SHARE_RECOVERY, &output.nonce, &signature);

        let new_count = dbtx
            .remove(&ISSUANCE_COUNTER, &output.denomination)
            .unwrap_or(0)
            .checked_add(1)
            .expect("Failed to increment issuance counter");

        dbtx.insert(&ISSUANCE_COUNTER, &output.denomination, &new_count);

        let next_index = get_recovery_count(dbtx);

        dbtx.insert(
            &RECOVERY_ITEM,
            &next_index,
            &RecoveryItem::Output {
                denomination: output.denomination,
                nonce_hash: output.nonce.consensus_hash(),
                tweak: output.tweak,
            },
        );

        let amount = output.amount();

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
            .iter(&ISSUANCE_COUNTER)
            .into_iter()
            .map(|(denomination, count)| {
                (
                    format!("IssuanceCounter({denomination:?})"),
                    -((denomination.amount().msats * count) as i64),
                )
            });

        audit.add_items(module_instance_id, items);
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            api_endpoint! {
                SIGNATURE_SHARES_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Mint, _context, range: fedimint_core::OutPointRange| -> Vec<BlindedSignatureShare> {
                    let db = module.db.clone();

                    let tx = db
                        .wait_key_check(&BLINDED_SIGNATURE_SHARE, &range.start_out_point(), std::convert::identity)
                        .await.1;

                    Ok(get_signature_shares(&tx, range))
                }
            },
            api_endpoint! {
                SIGNATURE_SHARES_RECOVERY_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Mint, _context, messages: Vec<tbs::BlindedMessage>| -> Vec<BlindedSignatureShare> {
                    let db = module.db.clone();
                    let tx = db.begin_read().await;
                    get_signature_shares_recovery(&tx, messages)
                }
            },
            api_endpoint! {
                RECOVERY_SLICE_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Mint, _context, range: (u64, u64)| -> Vec<RecoveryItem> {
                    let db = module.db.clone();
                    let tx = db.begin_read().await;
                    Ok(get_recovery_slice(&tx, range))
                }
            },
            api_endpoint! {
                RECOVERY_SLICE_HASH_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Mint, _context, range: (u64, u64)| -> bitcoin::hashes::sha256::Hash {
                    let db = module.db.clone();
                    let tx = db.begin_read().await;
                    Ok(get_recovery_slice(&tx, range).consensus_hash())
                }
            },
            api_endpoint! {
                RECOVERY_COUNT_ENDPOINT,
                ApiVersion::new(0, 1),
                async |module: &Mint, _context, _params: ()| -> u64 {
                    let db = module.db.clone();
                    let tx = db.begin_read().await;
                    Ok(get_recovery_count(&tx))
                }
            },
        ]
    }
}

fn get_signature_shares(
    tx: &fedimint_redb::v2::ReadTransaction,
    range: fedimint_core::OutPointRange,
) -> Vec<BlindedSignatureShare> {
    tx.range(
        &BLINDED_SIGNATURE_SHARE,
        range.start_out_point()..range.end_out_point(),
    )
    .into_iter()
    .map(|(_, v)| v)
    .collect()
}

fn get_signature_shares_recovery(
    tx: &fedimint_redb::v2::ReadTransaction,
    messages: Vec<tbs::BlindedMessage>,
) -> Result<Vec<BlindedSignatureShare>, ApiError> {
    let mut shares = Vec::new();

    for message in messages {
        let share =
            tx.get(&BLINDED_SIGNATURE_SHARE_RECOVERY, &message)
                .ok_or(ApiError::bad_request(
                    "No blinded signature share found".to_string(),
                ))?;

        shares.push(share);
    }

    Ok(shares)
}

fn get_recovery_count(dbtx: &impl IReadDatabaseTransactionOps) -> u64 {
    dbtx.iter(&RECOVERY_ITEM)
        .into_iter()
        .next_back()
        .map_or(0, |(idx, _)| idx + 1)
}

fn get_recovery_slice(
    tx: &fedimint_redb::v2::ReadTransaction,
    range: (u64, u64),
) -> Vec<RecoveryItem> {
    tx.range(&RECOVERY_ITEM, range.0..range.1)
        .into_iter()
        .map(|(_, v)| v)
        .collect()
}
