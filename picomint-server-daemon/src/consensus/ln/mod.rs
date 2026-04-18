#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::module_name_repetitions)]

pub use picomint_core::ln as common;

mod db;
mod rpc;

use anyhow::{Context, ensure};
use group::Curve;
use picomint_bitcoin_rpc::BitcoinRpcMonitor;
use picomint_core::bitcoin::Network;
use picomint_core::core::ModuleKind;
use picomint_core::module::audit::Audit;
use picomint_core::module::{ApiError, ApiRequestErased, InputMeta, TransactionItemAmounts};
use picomint_core::time::duration_since_epoch;
use picomint_core::util::SafeUrl;
use picomint_core::{Amount, InPoint, NumPeersExt, OutPoint, PeerId};
use picomint_core::ln::config::{
    LightningConfig, LightningConfigConsensus, LightningConfigPrivate,
};
use picomint_core::ln::endpoint_constants::{
    AWAIT_INCOMING_CONTRACT_ENDPOINT, AWAIT_INCOMING_CONTRACTS_ENDPOINT, AWAIT_PREIMAGE_ENDPOINT,
    CONSENSUS_BLOCK_COUNT_ENDPOINT, DECRYPTION_KEY_SHARE_ENDPOINT, GATEWAYS_ENDPOINT,
    OUTGOING_CONTRACT_EXPIRATION_ENDPOINT,
};
use picomint_core::ln::{
    LightningConsensusItem, LightningInput, LightningInputError, LightningOutput,
    LightningOutputError, OutgoingWitness,
};
use picomint_logging::LOG_MODULE_LN;
use picomint_redb::{Database, ReadTxRef, WriteTxRef};
use tpe::{PublicKeyShare, SecretKeyShare};
use tracing::trace;

use crate::config::dkg::DkgHandle;
use crate::config::poly::eval_poly_g1;
use crate::handler;

use self::db::{
    BLOCK_COUNT_VOTE, DECRYPTION_KEY_SHARE, GATEWAY, INCOMING_CONTRACT, INCOMING_CONTRACT_INDEX,
    INCOMING_CONTRACT_OUTPOINT, INCOMING_CONTRACT_STREAM, INCOMING_CONTRACT_STREAM_INDEX,
    OUTGOING_CONTRACT, PREIMAGE, UNIX_TIME_VOTE,
};

/// Run DKG for the lightning module, producing a fresh `LightningConfig` for
/// this peer.
pub async fn distributed_gen(
    peers: &DkgHandle<'_>,
    network: Network,
) -> anyhow::Result<LightningConfig> {
    let (polynomial, sks) = peers.run_dkg_g1().await?;

    Ok(LightningConfig {
        consensus: LightningConfigConsensus {
            tpe_agg_pk: tpe::AggregatePublicKey(polynomial[0].to_affine()),
            tpe_pks: peers
                .num_peers()
                .peer_ids()
                .map(|peer| (peer, PublicKeyShare(eval_poly_g1(&polynomial, &peer))))
                .collect(),
            input_fee: Amount::from_sats(1),
            output_fee: Amount::from_sats(1),
            network,
        },
        private: LightningConfigPrivate {
            sk: SecretKeyShare(sks),
        },
    })
}

/// Verify our private tpe share matches the public share in the consensus
/// config.
pub fn validate_config(identity: &PeerId, cfg: &LightningConfig) -> anyhow::Result<()> {
    ensure!(
        tpe::derive_pk_share(&cfg.private.sk)
            == *cfg
                .consensus
                .tpe_pks
                .get(identity)
                .context("Public key set has no key for our identity")?,
        "Preimge encryption secret key share does not match our public key share"
    );

    Ok(())
}

#[derive(Debug)]
pub struct Lightning {
    cfg: LightningConfig,
    db: Database,
    server_bitcoin_rpc_monitor: BitcoinRpcMonitor,
}

impl Lightning {
    pub fn new(
        cfg: LightningConfig,
        db: Database,
        server_bitcoin_rpc_monitor: BitcoinRpcMonitor,
    ) -> Self {
        Self {
            cfg,
            db,
            server_bitcoin_rpc_monitor,
        }
    }
}

impl Lightning {
    pub async fn consensus_proposal(&self, _dbtx: &ReadTxRef<'_>) -> Vec<LightningConsensusItem> {
        // We reduce the time granularity to deduplicate votes more often and not save
        // one consensus item every second.
        let mut items = vec![LightningConsensusItem::UnixTimeVote(
            60 * (duration_since_epoch().as_secs() / 60),
        )];

        if let Ok(block_count) = self.get_block_count() {
            trace!(target: LOG_MODULE_LN, ?block_count, "Proposing block count");
            items.push(LightningConsensusItem::BlockCountVote(block_count));
        }

        items
    }

    pub async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: LightningConsensusItem,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        trace!(target: LOG_MODULE_LN, ?consensus_item, "Processing consensus item proposal");

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
        }
    }

    pub async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &LightningInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, LightningInputError> {
        let (pub_key, amount) = match input {
            LightningInput::Outgoing(outpoint, outgoing_witness) => {
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
            LightningInput::Incoming(outpoint, agg_decryption_key) => {
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

    pub async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &LightningOutput,
        outpoint: OutPoint,
    ) -> Result<TransactionItemAmounts, LightningOutputError> {
        let amount = match output {
            LightningOutput::Outgoing(contract) => {
                dbtx.insert(&OUTGOING_CONTRACT, &outpoint, contract);

                contract.amount
            }
            LightningOutput::Incoming(contract) => {
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

    pub async fn audit(&self, dbtx: &WriteTxRef<'_>, audit: &mut Audit) {
        // Both incoming and outgoing contracts represent liabilities to the federation
        // since they are obligations to issue notes.
        audit.add_items(
            ModuleKind::Ln,
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
            ModuleKind::Ln,
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

    pub async fn handle_api(
        &self,
        method: &str,
        req: ApiRequestErased,
    ) -> Result<Vec<u8>, ApiError> {
        match method {
            CONSENSUS_BLOCK_COUNT_ENDPOINT => handler!(consensus_block_count, self, req).await,
            AWAIT_INCOMING_CONTRACT_ENDPOINT => handler!(await_incoming_contract, self, req).await,
            AWAIT_PREIMAGE_ENDPOINT => handler!(await_preimage, self, req).await,
            DECRYPTION_KEY_SHARE_ENDPOINT => handler!(decryption_key_share, self, req).await,
            OUTGOING_CONTRACT_EXPIRATION_ENDPOINT => {
                handler!(outgoing_contract_expiration, self, req).await
            }
            AWAIT_INCOMING_CONTRACTS_ENDPOINT => handler!(await_incoming_contracts, self, req).await,
            GATEWAYS_ENDPOINT => handler!(gateways, self, req).await,
            other => Err(ApiError::not_found(other.to_string())),
        }
    }
}

impl Lightning {
    fn get_block_count(&self) -> anyhow::Result<u64> {
        self.server_bitcoin_rpc_monitor
            .status()
            .map(|status| status.block_count)
            .context("Block count not available yet")
    }

    pub(crate) fn consensus_block_count(&self, dbtx: &impl picomint_redb::DbRead) -> u64 {
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

    pub(crate) fn consensus_unix_time(&self, dbtx: &impl picomint_redb::DbRead) -> u64 {
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

    pub async fn consensus_block_count_ui(&self) -> u64 {
        self.consensus_block_count(&self.db.begin_read().await)
    }

    pub async fn consensus_unix_time_ui(&self) -> u64 {
        self.consensus_unix_time(&self.db.begin_read().await)
    }

    pub async fn add_gateway_ui(&self, gateway: SafeUrl) -> bool {
        let tx = self.db.begin_write().await;
        let is_new_entry = tx.insert(&GATEWAY, &gateway, &()).is_none();
        tx.commit().await;
        is_new_entry
    }

    pub async fn remove_gateway_ui(&self, gateway: SafeUrl) -> bool {
        let tx = self.db.begin_write().await;
        let entry_existed = tx.remove(&GATEWAY, &gateway).is_some();
        tx.commit().await;
        entry_existed
    }

    pub async fn gateways_ui(&self) -> Vec<SafeUrl> {
        self.db
            .begin_read()
            .await
            .iter(&GATEWAY)
            .into_iter()
            .map(|(url, ())| url)
            .collect()
    }
}
