#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::module_name_repetitions)]

pub use fedimint_lnv2_common as common;

mod db;

use std::time::Duration;

use anyhow::{Context, anyhow, ensure};
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{
    IReadDatabaseTransactionOps, IReadDatabaseTransactionOpsTyped as _,
    IWriteDatabaseTransactionOpsTyped as _,
};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    ApiEndpoint, ApiError, ApiVersion, CoreConsensusVersion, InputMeta, ModuleConsensusVersion,
    ModuleInit, TransactionItemAmounts, api_endpoint,
};
use fedimint_core::task::timeout;
use fedimint_core::time::duration_since_epoch;
use fedimint_core::util::SafeUrl;
use fedimint_core::{
    Amount, InPoint, NumPeersExt, OutPoint, PeerId, apply, async_trait_maybe_send,
};
use fedimint_lnv2_common::config::{
    LightningClientConfig, LightningConfig, LightningConfigConsensus, LightningConfigPrivate,
};
use fedimint_lnv2_common::contracts::IncomingContract;
use fedimint_lnv2_common::endpoint_constants::{
    AWAIT_INCOMING_CONTRACT_ENDPOINT, AWAIT_INCOMING_CONTRACTS_ENDPOINT, AWAIT_PREIMAGE_ENDPOINT,
    CONSENSUS_BLOCK_COUNT_ENDPOINT, DECRYPTION_KEY_SHARE_ENDPOINT, GATEWAYS_ENDPOINT,
    OUTGOING_CONTRACT_EXPIRATION_ENDPOINT,
};
use fedimint_lnv2_common::{
    ContractId, LightningCommonInit, LightningConsensusItem, LightningInput, LightningInputError,
    LightningInputV0, LightningModuleTypes, LightningOutput, LightningOutputError,
    LightningOutputV0, MODULE_CONSENSUS_VERSION, OutgoingWitness,
};
use fedimint_logging::LOG_MODULE_LNV2;
use fedimint_redb::{Database, ReadTxRef, WriteTxRef};
use fedimint_server_core::bitcoin_rpc::ServerBitcoinRpcMonitor;
use fedimint_server_core::config::{PeerHandleOps, eval_poly_g1};
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
use group::Curve;
use tpe::{DecryptionKeyShare, PublicKeyShare, SecretKeyShare};
use tracing::trace;

use crate::db::{
    BLOCK_COUNT_VOTE, DECRYPTION_KEY_SHARE, GATEWAY, INCOMING_CONTRACT, INCOMING_CONTRACT_INDEX,
    INCOMING_CONTRACT_OUTPOINT, INCOMING_CONTRACT_STREAM, INCOMING_CONTRACT_STREAM_INDEX,
    OUTGOING_CONTRACT, PREIMAGE, UNIX_TIME_VOTE,
};

#[derive(Debug, Clone)]
pub struct LightningInit;

impl ModuleInit for LightningInit {
    type Common = LightningCommonInit;
}

#[apply(async_trait_maybe_send!)]
impl ServerModuleInit for LightningInit {
    type Module = Lightning;

    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(Lightning {
            cfg: args.cfg().to_typed()?,
            db: args.db().clone(),
            server_bitcoin_rpc_monitor: args.server_bitcoin_rpc_monitor(),
        })
    }

    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        let (polynomial, sks) = peers.run_dkg_g1().await?;

        let server = LightningConfig {
            consensus: LightningConfigConsensus {
                tpe_agg_pk: tpe::AggregatePublicKey(polynomial[0].to_affine()),
                tpe_pks: peers
                    .num_peers()
                    .peer_ids()
                    .map(|peer| (peer, PublicKeyShare(eval_poly_g1(&polynomial, &peer))))
                    .collect(),
                input_fee: Amount::from_sats(1),
                output_fee: Amount::from_sats(1),
                network: args.network,
            },
            private: LightningConfigPrivate {
                sk: SecretKeyShare(sks),
            },
        };

        Ok(server.to_erased())
    }

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()> {
        let config = config.to_typed::<LightningConfig>()?;

        ensure!(
            tpe::derive_pk_share(&config.private.sk)
                == *config
                    .consensus
                    .tpe_pks
                    .get(identity)
                    .context("Public key set has no key for our identity")?,
            "Preimge encryption secret key share does not match our public key share"
        );

        Ok(())
    }

    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<LightningClientConfig> {
        let config = LightningConfigConsensus::from_erased(config)?;
        Ok(LightningClientConfig {
            tpe_agg_pk: config.tpe_agg_pk,
            tpe_pks: config.tpe_pks,
            input_fee: config.input_fee,
            output_fee: config.output_fee,
            network: config.network,
        })
    }

}

#[derive(Debug)]
pub struct Lightning {
    cfg: LightningConfig,
    db: Database,
    server_bitcoin_rpc_monitor: ServerBitcoinRpcMonitor,
}

#[apply(async_trait_maybe_send!)]
impl ServerModule for Lightning {
    type Common = LightningModuleTypes;
    type Init = LightningInit;

    async fn consensus_proposal(&self, _dbtx: &ReadTxRef<'_>) -> Vec<LightningConsensusItem> {
        // We reduce the time granularity to deduplicate votes more often and not save
        // one consensus item every second.
        let mut items = vec![LightningConsensusItem::UnixTimeVote(
            60 * (duration_since_epoch().as_secs() / 60),
        )];

        if let Ok(block_count) = self.get_block_count() {
            trace!(target: LOG_MODULE_LNV2, ?block_count, "Proposing block count");
            items.push(LightningConsensusItem::BlockCountVote(block_count));
        }

        items
    }

    async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: LightningConsensusItem,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        trace!(target: LOG_MODULE_LNV2, ?consensus_item, "Processing consensus item proposal");

        match consensus_item {
            LightningConsensusItem::BlockCountVote(vote) => {
                let current_vote = dbtx.insert(&BLOCK_COUNT_VOTE, &peer, &vote).unwrap_or(0);

                ensure!(current_vote < vote, "Block count vote is redundant");

                Ok(())
            }
            LightningConsensusItem::UnixTimeVote(vote) => {
                let current_vote = dbtx.insert(&UNIX_TIME_VOTE, &peer, &vote).unwrap_or(0);

                ensure!(current_vote < vote, "Unix time vote is redundant");

                Ok(())
            }
            LightningConsensusItem::Default { variant, .. } => Err(anyhow!(
                "Received lnv2 consensus item with unknown variant {variant}"
            )),
        }
    }

    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &LightningInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, LightningInputError> {
        let (pub_key, amount) = match input.ensure_v0_ref()? {
            LightningInputV0::Outgoing(outpoint, outgoing_witness) => {
                let contract = dbtx
                    .remove(&OUTGOING_CONTRACT, outpoint)
                    .ok_or(LightningInputError::UnknownContract)?;

                let pub_key = match outgoing_witness {
                    OutgoingWitness::Claim(preimage) => {
                        if contract.expiration <= self.consensus_block_count(dbtx) {
                            return Err(LightningInputError::Expired);
                        }

                        if !contract.verify_preimage(preimage) {
                            return Err(LightningInputError::InvalidPreimage);
                        }

                        dbtx.insert(&PREIMAGE, outpoint, preimage);

                        contract.claim_pk
                    }
                    OutgoingWitness::Refund => {
                        if contract.expiration > self.consensus_block_count(dbtx) {
                            return Err(LightningInputError::NotExpired);
                        }

                        contract.refund_pk
                    }
                    OutgoingWitness::Cancel(forfeit_signature) => {
                        if !contract.verify_forfeit_signature(forfeit_signature) {
                            return Err(LightningInputError::InvalidForfeitSignature);
                        }

                        contract.refund_pk
                    }
                };

                (pub_key, contract.amount)
            }
            LightningInputV0::Incoming(outpoint, agg_decryption_key) => {
                let contract = dbtx
                    .remove(&INCOMING_CONTRACT, outpoint)
                    .ok_or(LightningInputError::UnknownContract)?;

                let index = dbtx
                    .remove(&INCOMING_CONTRACT_INDEX, outpoint)
                    .expect("Incoming contract index should exist");

                dbtx.remove(&INCOMING_CONTRACT_STREAM, &index);

                if !contract
                    .verify_agg_decryption_key(&self.cfg.consensus.tpe_agg_pk, agg_decryption_key)
                {
                    return Err(LightningInputError::InvalidDecryptionKey);
                }

                let pub_key = match contract.decrypt_preimage(agg_decryption_key) {
                    Some(..) => contract.commitment.claim_pk,
                    None => contract.commitment.refund_pk,
                };

                (pub_key, contract.commitment.amount)
            }
        };

        Ok(InputMeta {
            amount: TransactionItemAmounts {
                amount,
                fee: self.cfg.consensus.input_fee,
            },
            pub_key,
        })
    }

    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &LightningOutput,
        outpoint: OutPoint,
    ) -> Result<TransactionItemAmounts, LightningOutputError> {
        let amount = match output.ensure_v0_ref()? {
            LightningOutputV0::Outgoing(contract) => {
                dbtx.insert(&OUTGOING_CONTRACT, &outpoint, contract);

                contract.amount
            }
            LightningOutputV0::Incoming(contract) => {
                if !contract.verify() {
                    return Err(LightningOutputError::InvalidContract);
                }

                if contract.commitment.expiration <= self.consensus_unix_time(dbtx) {
                    return Err(LightningOutputError::ContractExpired);
                }

                dbtx.insert(&INCOMING_CONTRACT, &outpoint, contract);

                dbtx.insert(
                    &INCOMING_CONTRACT_OUTPOINT,
                    &contract.contract_id(),
                    &outpoint,
                );

                let stream_index = dbtx.get(&INCOMING_CONTRACT_STREAM_INDEX, &()).unwrap_or(0);

                dbtx.insert(&INCOMING_CONTRACT_STREAM, &stream_index, contract);

                dbtx.insert(&INCOMING_CONTRACT_INDEX, &outpoint, &stream_index);

                dbtx.insert(&INCOMING_CONTRACT_STREAM_INDEX, &(), &(stream_index + 1));

                let dk_share = contract.create_decryption_key_share(&self.cfg.private.sk);

                dbtx.insert(&DECRYPTION_KEY_SHARE, &outpoint, &dk_share);

                contract.commitment.amount
            }
        };

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
        // Both incoming and outgoing contracts represent liabilities to the federation
        // since they are obligations to issue notes.
        audit.add_items(
            module_instance_id,
            dbtx.iter(&OUTGOING_CONTRACT)
                .into_iter()
                .map(|(outpoint, contract)| {
                    (
                        format!("OutgoingContract({outpoint:?})"),
                        -(contract.amount.msats as i64),
                    )
                }),
        );

        audit.add_items(
            module_instance_id,
            dbtx.iter(&INCOMING_CONTRACT)
                .into_iter()
                .map(|(outpoint, contract)| {
                    (
                        format!("IncomingContract({outpoint:?})"),
                        -(contract.commitment.amount.msats as i64),
                    )
                }),
        );
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        vec![
            api_endpoint! {
                CONSENSUS_BLOCK_COUNT_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, _params : () | -> u64 {
                    let db = module.db.clone();
                    let tx = db.begin_read().await;

                    Ok(module.consensus_block_count(&tx))
                }
            },
            api_endpoint! {
                AWAIT_INCOMING_CONTRACT_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, params: (ContractId, u64) | -> Option<OutPoint> {
                    let db = module.db.clone();

                    Ok(module.await_incoming_contract(db, params.0, params.1).await)
                }
            },
            api_endpoint! {
                AWAIT_PREIMAGE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, params: (OutPoint, u64)| -> Option<[u8; 32]> {
                    let db = module.db.clone();

                    Ok(module.await_preimage(db, params.0, params.1).await)
                }
            },
            api_endpoint! {
                DECRYPTION_KEY_SHARE_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, params: OutPoint| -> DecryptionKeyShare {
                    let share = module
                        .db
                        .begin_read()
                        .await
                        .get(&DECRYPTION_KEY_SHARE, &params)
                        .ok_or(ApiError::bad_request("No decryption key share found".to_string()))?;

                    Ok(share)
                }
            },
            api_endpoint! {
                OUTGOING_CONTRACT_EXPIRATION_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, outpoint: OutPoint| -> Option<(ContractId, u64)> {
                    let db = module.db.clone();

                    Ok(module.outgoing_contract_expiration(db, outpoint).await)
                }
            },
            api_endpoint! {
                AWAIT_INCOMING_CONTRACTS_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, params: (u64, usize)| -> (Vec<IncomingContract>, u64) {
                    let db = module.db.clone();

                    if params.1 == 0 {
                        return Err(ApiError::bad_request("Batch size must be greater than 0".to_string()));
                    }

                    Ok(module.await_incoming_contracts(db, params.0, params.1).await)
                }
            },
            api_endpoint! {
                GATEWAYS_ENDPOINT,
                ApiVersion::new(0, 0),
                async |module: &Lightning, _params : () | -> Vec<SafeUrl> {
                    let db = module.db.clone();

                    Ok(Lightning::gateways(db).await)
                }
            },
        ]
    }
}

impl Lightning {
    fn get_block_count(&self) -> anyhow::Result<u64> {
        self.server_bitcoin_rpc_monitor
            .status()
            .map(|status| status.block_count)
            .context("Block count not available yet")
    }

    fn consensus_block_count(&self, dbtx: &impl IReadDatabaseTransactionOps) -> u64 {
        let num_peers = self.cfg.consensus.tpe_pks.to_num_peers();

        let mut counts = dbtx
            .iter(&BLOCK_COUNT_VOTE)
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<u64>>();

        counts.sort_unstable();

        counts.reverse();

        assert!(counts.last() <= counts.first());

        // The block count we select guarantees that any threshold of correct peers can
        // increase the consensus block count and any consensus block count has been
        // confirmed by a threshold of peers.

        counts.get(num_peers.threshold() - 1).copied().unwrap_or(0)
    }

    fn consensus_unix_time(&self, dbtx: &impl IReadDatabaseTransactionOps) -> u64 {
        let num_peers = self.cfg.consensus.tpe_pks.to_num_peers();

        let mut times = dbtx
            .iter(&UNIX_TIME_VOTE)
            .into_iter()
            .map(|(_, v)| v)
            .collect::<Vec<u64>>();

        times.sort_unstable();

        times.reverse();

        assert!(times.last() <= times.first());

        times.get(num_peers.threshold() - 1).copied().unwrap_or(0)
    }

    async fn await_incoming_contract(
        &self,
        db: Database,
        contract_id: ContractId,
        expiration: u64,
    ) -> Option<OutPoint> {
        loop {
            // Wait for the contract to appear, or time out periodically to check
            // expiration.
            let wait = db.wait_key(&INCOMING_CONTRACT_OUTPOINT, &contract_id);

            if let Ok((outpoint, _tx)) = timeout(Duration::from_secs(10), wait).await {
                return Some(outpoint);
            }

            // Timed out; check whether the contract arrived or has expired.
            let tx = db.begin_read().await;

            if let Some(outpoint) = tx.get(&INCOMING_CONTRACT_OUTPOINT, &contract_id) {
                return Some(outpoint);
            }

            if expiration <= self.consensus_unix_time(&tx) {
                return None;
            }
        }
    }

    async fn await_preimage(
        &self,
        db: Database,
        outpoint: OutPoint,
        expiration: u64,
    ) -> Option<[u8; 32]> {
        loop {
            let wait = db.wait_key(&PREIMAGE, &outpoint);

            if let Ok((preimage, _tx)) = timeout(Duration::from_secs(10), wait).await {
                return Some(preimage);
            }

            let tx = db.begin_read().await;

            if let Some(preimage) = tx.get(&PREIMAGE, &outpoint) {
                return Some(preimage);
            }

            if expiration <= self.consensus_block_count(&tx) {
                return None;
            }
        }
    }

    async fn outgoing_contract_expiration(
        &self,
        db: Database,
        outpoint: OutPoint,
    ) -> Option<(ContractId, u64)> {
        let tx = db.begin_read().await;

        let contract = tx.get(&OUTGOING_CONTRACT, &outpoint)?;

        let consensus_block_count = self.consensus_block_count(&tx);

        let expiration = contract.expiration.saturating_sub(consensus_block_count);

        Some((contract.contract_id(), expiration))
    }

    async fn await_incoming_contracts(
        &self,
        db: Database,
        start: u64,
        n: usize,
    ) -> (Vec<IncomingContract>, u64) {
        let filter = |next_index: Option<u64>| next_index.filter(|i| *i > start);

        let (mut next_index, tx) = db
            .wait_key_check(&INCOMING_CONTRACT_STREAM_INDEX, &(), filter)
            .await;

        let mut contracts = Vec::with_capacity(n);

        for (key, contract) in tx
            .range(&INCOMING_CONTRACT_STREAM, start..u64::MAX)
            .into_iter()
            .take(n)
        {
            contracts.push(contract);
            next_index = key + 1;
        }

        (contracts, next_index)
    }

    async fn add_gateway(db: Database, gateway: SafeUrl) -> bool {
        let tx = db.begin_write().await;

        let is_new_entry = tx.insert(&GATEWAY, &gateway, &()).is_none();

        tx.commit().await;

        is_new_entry
    }

    async fn remove_gateway(db: Database, gateway: SafeUrl) -> bool {
        let tx = db.begin_write().await;

        let entry_existed = tx.remove(&GATEWAY, &gateway).is_some();

        tx.commit().await;

        entry_existed
    }

    async fn gateways(db: Database) -> Vec<SafeUrl> {
        db.begin_read()
            .await
            .iter(&GATEWAY)
            .into_iter()
            .map(|(url, ())| url)
            .collect()
    }

    pub async fn consensus_block_count_ui(&self) -> u64 {
        self.consensus_block_count(&self.db.begin_read().await)
    }

    pub async fn consensus_unix_time_ui(&self) -> u64 {
        self.consensus_unix_time(&self.db.begin_read().await)
    }

    pub async fn add_gateway_ui(&self, gateway: SafeUrl) -> bool {
        Self::add_gateway(self.db.clone(), gateway).await
    }

    pub async fn remove_gateway_ui(&self, gateway: SafeUrl) -> bool {
        Self::remove_gateway(self.db.clone(), gateway).await
    }

    pub async fn gateways_ui(&self) -> Vec<SafeUrl> {
        Self::gateways(self.db.clone()).await
    }
}
