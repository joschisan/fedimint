use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail};
use async_channel::Receiver;
use fedimint_api_client::session_outcome::{AcceptedItem, SessionOutcome, SignedSessionOutcome};
use fedimint_api_client::transaction::ConsensusItem;
use fedimint_core::encoding::Decodable;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::net::peers::{DynP2PConnections, Recipient};
use fedimint_core::runtime::spawn;
use fedimint_core::secp256k1::schnorr;
use fedimint_core::task::{TaskGroup, TaskHandle, sleep};
use fedimint_core::timing::TimeReporter;
use fedimint_core::util::{FmtCompact as _, FmtCompactAnyhow as _};
use fedimint_core::{NumPeers, NumPeersExt, PeerId, timing};
use fedimint_redb::{Database, ReadTransaction, WriteTransaction};
use fedimint_server_core::{ServerModuleRegistry, ServerModuleRegistryExt};
use rand::Rng;
use rand::seq::IteratorRandom;
use tokio::sync::watch;
use tracing::{Level, debug, error, info, instrument, trace};

use crate::LOG_CONSENSUS;
use crate::config::ServerConfig;
use crate::consensus::aleph_bft::backup::{BackupReader, BackupWriter};
use crate::consensus::aleph_bft::data_provider::{DataProvider, UnitData};
use crate::consensus::aleph_bft::finalization_handler::{FinalizationHandler, OrderedUnit};
use crate::consensus::aleph_bft::keychain::Keychain;
use crate::consensus::aleph_bft::network::Network;
use crate::consensus::aleph_bft::spawner::Spawner;
use crate::consensus::db::{
    ACCEPTED_ITEM, ACCEPTED_TRANSACTION, ALEPH_UNITS, SIGNED_SESSION_OUTCOME,
};
use crate::consensus::debug::DebugConsensusItem;
use crate::consensus::transaction::process_transaction_with_dbtx;
use crate::p2p::P2PMessage;

/// Runs the main server consensus loop
pub struct ConsensusEngine {
    pub modules: ServerModuleRegistry,
    pub db: Database,
    pub cfg: ServerConfig,
    pub submission_receiver: Receiver<ConsensusItem>,
    pub shutdown_receiver: watch::Receiver<Option<u64>>,
    pub connections: DynP2PConnections<P2PMessage>,
    pub ci_status_senders: BTreeMap<PeerId, watch::Sender<Option<u64>>>,
    pub ord_latency_sender: watch::Sender<Option<Duration>>,
    pub task_group: TaskGroup,
}

impl ConsensusEngine {
    fn num_peers(&self) -> NumPeers {
        self.cfg.consensus.broadcast_public_keys.to_num_peers()
    }

    fn identity(&self) -> PeerId {
        self.cfg.local.identity
    }

    #[instrument(target = LOG_CONSENSUS, name = "run", skip_all, fields(id=%self.cfg.local.identity))]
    pub async fn run(self) -> anyhow::Result<()> {
        self.run_consensus(self.task_group.make_handle()).await
    }

    pub async fn run_consensus(&self, task_handle: TaskHandle) -> anyhow::Result<()> {
        // We need four peers to run the atomic broadcast
        assert!(self.num_peers().total() >= 4);

        while !task_handle.is_shutting_down() {
            let session_index = self.get_finished_session_count().await;

            let is_recovery = self.is_recovery().await;

            info!(
                target: LOG_CONSENSUS,
                session_index,
                is_recovery,
                "Starting consensus session"
            );

            if self
                .run_session(self.connections.clone(), session_index)
                .await
                .is_none()
            {
                return Ok(());
            }

            info!(target: LOG_CONSENSUS, ?session_index, "Completed consensus session");

            if Some(session_index) == self.shutdown_receiver.borrow().to_owned() {
                info!(target: LOG_CONSENSUS, "Initiating shutdown, waiting for peers to complete the session...");

                sleep(Duration::from_mins(1)).await;

                break;
            }
        }

        info!(target: LOG_CONSENSUS, "Consensus task shut down");

        Ok(())
    }

    pub async fn run_session(
        &self,
        connections: DynP2PConnections<P2PMessage>,
        session_index: u64,
    ) -> Option<()> {
        // In order to bound a sessions RAM consumption we need to bound its number of
        // units and therefore its number of rounds. Since we use a session to
        // create a naive secp256k1 threshold signature for the header of session
        // outcome we have to guarantee that an attacker cannot exhaust our
        // memory by preventing the creation of a threshold signature, thereby
        // keeping the session open indefinitely. Hence, after a certain round
        // index, we increase the delay between rounds exponentially such that
        // the end of the aleph bft session would only be reached after a minimum
        // of 10 years. In case of such an attack the broadcast stops ordering any
        // items until the attack subsides as no items are ordered while the
        // signatures are collected. The maximum RAM consumption of the aleph bft
        // broadcast instance is therefore bound by:
        //
        // self.keychain.peer_count()
        //      * (broadcast_rounds_per_session + EXP_SLOWDOWN_ROUNDS)
        //      * ALEPH_BFT_UNIT_BYTE_LIMIT

        const EXP_SLOWDOWN_ROUNDS: u16 = 1000;
        const BASE: f64 = 1.02;

        let rounds_per_session = self.cfg.consensus.broadcast_rounds_per_session;
        let round_delay = f64::from(self.cfg.local.broadcast_round_delay_ms);

        let mut delay_config = aleph_bft::default_delay_config();

        delay_config.unit_creation_delay = Arc::new(move |round_index| {
            let delay = if round_index == 0 {
                0.0
            } else {
                round_delay
                    * BASE.powf(round_index.saturating_sub(rounds_per_session as usize) as f64)
                    * rand::thread_rng().gen_range(0.5..=1.5)
            };

            Duration::from_millis(delay.round() as u64)
        });

        let config = aleph_bft::create_config(
            self.num_peers().total().into(),
            self.identity().to_usize().into(),
            session_index,
            self.cfg
                .consensus
                .broadcast_rounds_per_session
                .checked_add(EXP_SLOWDOWN_ROUNDS)
                .expect("Rounds per session exceed maximum of u16::Max - EXP_SLOWDOWN_ROUNDS"),
            delay_config,
            Duration::from_hours(87600),
        )
        .expect("The exponential slowdown exceeds 10 years");

        // we can use an unbounded channel here since the number and size of units
        // ordered in a single aleph session is bounded as described above
        let (unit_data_sender, unit_data_receiver) = async_channel::unbounded();
        let (timestamp_sender, timestamp_receiver) = async_channel::unbounded();
        let (terminator_sender, terminator_receiver) = futures::channel::oneshot::channel();

        // Create channels for P2P session sync
        let (signed_outcomes_sender, signed_outcomes_receiver) = async_channel::unbounded();
        let (signatures_sender, signatures_receiver) = async_channel::unbounded();

        let aleph_handle = spawn(
            "aleph run session",
            aleph_bft::run_session(
                config,
                aleph_bft::LocalIO::new(
                    DataProvider::new(
                        self.submission_receiver.clone(),
                        timestamp_sender,
                        self.is_recovery().await,
                    ),
                    FinalizationHandler::new(unit_data_sender),
                    BackupWriter::new(self.db.clone()).await,
                    BackupReader::new(self.db.clone()),
                ),
                Network::new(
                    connections.clone(),
                    signed_outcomes_sender,
                    signatures_sender,
                    self.db.clone(),
                ),
                Keychain::new(&self.cfg),
                Spawner::new(self.task_group.make_subgroup()),
                aleph_bft::Terminator::create_root(terminator_receiver, "Terminator"),
            ),
        );

        self.ord_latency_sender.send_replace(None);

        let signed_session_outcome = self
            .complete_signed_session_outcome(
                session_index,
                unit_data_receiver,
                timestamp_receiver,
                signed_outcomes_receiver,
                signatures_receiver,
                connections,
            )
            .await?;

        assert!(
            self.validate_signed_session_outcome(&signed_session_outcome, session_index),
            "Our created signed session outcome fails validation"
        );

        info!(target: LOG_CONSENSUS, ?session_index, "Terminating Aleph BFT session");

        // We can terminate the session instead of waiting for other peers to complete
        // it since they can always download the signed session outcome from us
        terminator_sender.send(()).ok();
        aleph_handle.await.ok();

        // This method removes the backup of the current session from the database
        // and therefore has to be called after we have waited for the session to
        // shut down, or we risk write-write conflicts with the UnitSaver
        self.complete_session(session_index, signed_session_outcome)
            .await;

        Some(())
    }

    async fn is_recovery(&self) -> bool {
        !self.db.begin_read().await.iter(&ALEPH_UNITS).is_empty()
    }

    pub async fn complete_signed_session_outcome(
        &self,
        session_index: u64,
        ordered_unit_receiver: Receiver<OrderedUnit>,
        timestamp_receiver: Receiver<Instant>,
        signed_outcomes_receiver: Receiver<(PeerId, SignedSessionOutcome)>,
        signatures_receiver: Receiver<(PeerId, schnorr::Signature)>,
        connections: DynP2PConnections<P2PMessage>,
    ) -> Option<SignedSessionOutcome> {
        // It is guaranteed that aleph bft will always replay all previously processed
        // items from the current session from index zero
        let mut item_index = 0;

        // We request the signed session outcome every three seconds from a random peer
        let mut index_broadcast_interval = tokio::time::interval(Duration::from_secs(3));

        // We build a session outcome out of the ordered batches until either we have
        // processed broadcast_rounds_per_session rounds or a threshold signed
        // session outcome is obtained from our peers
        loop {
            tokio::select! {
                result = ordered_unit_receiver.recv() => {
                    let ordered_unit = result.ok()?;

                    if ordered_unit.round >= self.cfg.consensus.broadcast_rounds_per_session {
                        info!(
                            target: LOG_CONSENSUS,
                            session_index,
                            "Reached Aleph BFT round limit, stopping item collection"
                        );
                        break;
                    }

                    if let Some(UnitData(bytes)) = ordered_unit.data {
                        if ordered_unit.creator == self.identity() {
                            match timestamp_receiver.try_recv() {
                                Ok(timestamp) => {
                                    let latency = match *self.ord_latency_sender.borrow() {
                                        Some(latency) => (9 * latency +  timestamp.elapsed()) / 10,
                                        None => timestamp.elapsed()
                                    };

                                    self.ord_latency_sender.send_replace(Some(latency));
                                }
                                Err(err) => {
                                    debug!(target: LOG_CONSENSUS, err = %err.fmt_compact(), "Missing submission timestamp. This is normal in recovery");
                                }
                            }
                        }

                        match Vec::<ConsensusItem>::consensus_decode_whole(&bytes, &self.decoders()) {
                            Ok(items) => {
                                for item in items {
                                    if let Ok(()) = self.process_consensus_item(
                                        session_index,
                                        item_index,
                                        item.clone(),
                                        ordered_unit.creator
                                    ).await {
                                        item_index += 1;
                                    }
                                }
                            }
                            Err(err) => {
                                error!(
                                    target: LOG_CONSENSUS,
                                    session_index,
                                    peer = %ordered_unit.creator,
                                    err = %err.fmt_compact(),
                                    "Failed to decode consensus items from peer"
                                );
                            }
                        }
                    }
                },
                result = signed_outcomes_receiver.recv() => {
                    let (peer_id, p2p_outcome) = result.ok()?;

                    // Validate signatures
                    if self.validate_signed_session_outcome(&p2p_outcome, session_index) {
                        info!(
                            target: LOG_CONSENSUS,
                            session_index,
                            peer_id = %peer_id,
                            "Received SignedSessionOutcome via P2P while collection signatures"
                        );

                        let pending_accepted_items = self.pending_accepted_items().await;

                        // this panics if we have more accepted items than the signed session outcome
                        let (processed, unprocessed) = p2p_outcome
                            .session_outcome
                            .items
                            .split_at(pending_accepted_items.len());

                        info!(
                            target: LOG_CONSENSUS,
                            ?session_index,
                            processed = %processed.len(),
                            unprocessed = %unprocessed.len(),
                            "Processing remaining items..."
                        );

                        assert!(
                            processed.iter().eq(pending_accepted_items.iter()),
                            "Consensus Failure: pending accepted items disagree with federation consensus"
                        );

                        for (accepted_item, item_index) in unprocessed.iter().zip(processed.len()..) {
                            if let Err(err) = self.process_consensus_item(
                                session_index,
                                item_index as u64,
                                accepted_item.item.clone(),
                                accepted_item.peer
                            ).await {
                                panic!(
                                    "Consensus Failure: rejected item accepted by federation consensus: {accepted_item:?}, items: {}+{}, session_idx: {session_index}, item_idx: {item_index}, err: {err}",
                                    processed.len(),
                                    unprocessed.len(),
                                );
                            }
                        }

                        info!(
                            target: LOG_CONSENSUS,
                            ?session_index,
                            peer_id = %peer_id,
                            "Successfully recovered session via P2P"
                        );

                        return Some(p2p_outcome);
                    }

                    debug!(
                        target: LOG_CONSENSUS,
                        %peer_id,
                        "Invalid P2P SignedSessionOutcome"
                    );
                }
                _ = index_broadcast_interval.tick() => {
                    connections.send(
                        Recipient::Peer(self.random_peer()),
                        P2PMessage::SessionIndex(session_index),
                    );
                }
            }
        }

        let items = self.pending_accepted_items().await;

        assert_eq!(item_index, items.len() as u64);

        info!(target: LOG_CONSENSUS, ?session_index, ?item_index, "Processed all items for session");

        let session_outcome = SessionOutcome { items };

        let header = session_outcome.header(session_index);

        info!(
            target: LOG_CONSENSUS,
            ?session_index,
            "Signing session header..."
        );

        let keychain = Keychain::new(&self.cfg);

        let our_signature = keychain.sign_schnorr(&header);

        let mut signatures = BTreeMap::from_iter([(self.identity(), our_signature)]);

        // We request the session signature every second to all peers
        let mut signature_broadcast_interval = tokio::time::interval(Duration::from_secs(1));

        // We collect the ordered signatures until we either obtain a threshold
        // signature or a signed session outcome arrives from our peers
        while signatures.len() < self.num_peers().threshold() {
            tokio::select! {
                result = signatures_receiver.recv() => {
                    let (peer_id, signature) = result.ok()?;

                    if keychain.verify_schnorr(&header, &signature, peer_id) {
                        signatures.insert(peer_id, signature);

                        info!(
                            target: LOG_CONSENSUS,
                            session_index,
                            peer_id = %peer_id,
                            "Collected signature from peer via P2P"
                        );
                    }

                    debug!(
                        target: LOG_CONSENSUS,
                        session_index,
                        peer_id = %peer_id,
                        "Invalid P2P signature from peer"
                    );
                }
                result = signed_outcomes_receiver.recv() => {
                    let (peer_id, p2p_outcome) = result.ok()?;

                    if self.validate_signed_session_outcome(&p2p_outcome, session_index) {
                        assert_eq!(
                            header,
                            p2p_outcome.session_outcome.header(session_index),
                            "Consensus Failure: header disagrees with federation consensus"
                        );

                        info!(
                            target: LOG_CONSENSUS,
                            session_index,
                            %peer_id,
                            "Recovered session via P2P while collecting signatures"
                        );

                        return Some(p2p_outcome);
                    }

                    debug!(
                        target: LOG_CONSENSUS,
                        %peer_id,
                        "Invalid P2P SignedSessionOutcome"
                    );
                }
                _ = signature_broadcast_interval.tick() => {
                    connections.send(
                        Recipient::Everyone,
                        P2PMessage::SessionSignature(our_signature),
                    );
                }
                _ = index_broadcast_interval.tick() => {
                    connections.send(
                        Recipient::Peer(self.random_peer()),
                        P2PMessage::SessionIndex(session_index),
                    );
                }
            }
        }

        info!(
            target: LOG_CONSENSUS,
            session_index,
            "Successfully collected threshold of signatures"
        );

        Some(SignedSessionOutcome {
            session_outcome,
            signatures,
        })
    }

    /// Returns a random peer ID excluding ourselves
    fn random_peer(&self) -> PeerId {
        self.num_peers()
            .peer_ids()
            .filter(|p| *p != self.identity())
            .choose(&mut rand::thread_rng())
            .expect("We have at least three peers")
    }

    /// Validate a SignedSessionOutcome received via P2P
    fn validate_signed_session_outcome(
        &self,
        outcome: &SignedSessionOutcome,
        session_index: u64,
    ) -> bool {
        if outcome.signatures.len() != self.num_peers().threshold() {
            return false;
        }

        let keychain = Keychain::new(&self.cfg);
        let header = outcome.session_outcome.header(session_index);

        outcome
            .signatures
            .iter()
            .all(|(signer_id, sig)| keychain.verify_schnorr(&header, sig, *signer_id))
    }

    fn decoders(&self) -> ModuleDecoderRegistry {
        self.modules.decoder_registry()
    }

    pub async fn pending_accepted_items(&self) -> Vec<AcceptedItem> {
        self.db
            .begin_read()
            .await
            .iter(&ACCEPTED_ITEM)
            .into_iter()
            .map(|(_, item)| item)
            .collect()
    }

    pub async fn complete_session(
        &self,
        session_index: u64,
        signed_session_outcome: SignedSessionOutcome,
    ) {
        let tx = self.db.begin_write().await;

        tx.as_ref().delete_table(&ALEPH_UNITS);
        tx.as_ref().delete_table(&ACCEPTED_ITEM);

        assert!(
            tx.insert(
                &SIGNED_SESSION_OUTCOME,
                &session_index,
                &signed_session_outcome,
            )
            .is_none(),
            "We tried to overwrite a signed session outcome"
        );

        tx.commit().await;
    }

    #[instrument(target = LOG_CONSENSUS, skip(self, item), level = "info")]
    pub async fn process_consensus_item(
        &self,
        session_index: u64,
        item_index: u64,
        item: ConsensusItem,
        peer: PeerId,
    ) -> anyhow::Result<()> {
        let _timing /* logs on drop */ = timing::TimeReporter::new("process_consensus_item").level(Level::TRACE);

        trace!(
            target: LOG_CONSENSUS,
            %peer,
            item = ?DebugConsensusItem(&item),
            "Processing consensus item"
        );

        self.ci_status_senders
            .get(&peer)
            .expect("No ci status sender for peer")
            .send_replace(Some(session_index));

        let tx = self.db.begin_write().await;

        // When we recover from a mid-session crash aleph bft will replay the units that
        // were already processed before the crash. We therefore skip all consensus
        // items until we have seen every previously accepted items again.
        if let Some(existing_item) = tx.get(&ACCEPTED_ITEM, &item_index) {
            if existing_item.item == item && existing_item.peer == peer {
                return Ok(());
            }

            bail!(
                "Item was discarded previously: existing: {existing_item:?} {}, current: {item:?}, {peer}",
                existing_item.peer
            );
        }

        self.process_consensus_item_with_db_transaction(&tx, item.clone(), peer)
            .await
            .inspect_err(|err| {
                // Rejected items are very common, so only trace level
                trace!(
                    target: LOG_CONSENSUS,
                    %peer,
                    item = ?DebugConsensusItem(&item),
                    err = %err.fmt_compact_anyhow(),
                    "Rejected consensus item"
                );
            })?;

        tx.insert(
            &ACCEPTED_ITEM,
            &item_index,
            &AcceptedItem {
                item: item.clone(),
                peer,
            },
        );

        debug!(
            target: LOG_CONSENSUS,
            %peer,
            item = ?DebugConsensusItem(&item),
            "Processed consensus item"
        );
        let mut audit = Audit::default();

        for (module_instance_id, _kind, module) in self.modules.iter_modules() {
            let _module_audit_timing =
                TimeReporter::new(format!("audit module {module_instance_id}")).level(Level::TRACE);

            let view = tx.isolate(format!("module-{module_instance_id}"));

            module.audit(&view, &mut audit, module_instance_id).await;
        }

        assert!(
            audit
                .net_assets()
                .expect("Overflow while checking balance sheet")
                .milli_sat
                >= 0,
            "Balance sheet of the fed has gone negative, this should never happen! {audit}"
        );

        tx.commit().await;

        Ok(())
    }

    async fn process_consensus_item_with_db_transaction(
        &self,
        tx: &WriteTransaction,
        consensus_item: ConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        // We rely on decoding rejecting any unknown module instance ids to avoid
        // peer-triggered panic here
        self.decoders().assert_reject_mode();

        match consensus_item {
            ConsensusItem::Module(module_item) => {
                let instance_id = module_item.module_instance_id();

                let view = tx.isolate(format!("module-{instance_id}"));

                self.modules
                    .get_expect(instance_id)
                    .process_consensus_item(&view, &module_item, peer_id)
                    .await
            }
            ConsensusItem::Transaction(transaction) => {
                let txid = transaction.tx_hash();
                if tx.get(&ACCEPTED_TRANSACTION, &txid).is_some() {
                    debug!(
                        target: LOG_CONSENSUS,
                        %txid,
                        "Transaction already accepted"
                    );
                    bail!("Transaction is already accepted");
                }

                let modules_ids = transaction
                    .outputs
                    .iter()
                    .map(|o| o.module_instance_id())
                    .collect::<Vec<_>>();

                process_transaction_with_dbtx(self.modules.clone(), tx, &transaction)
                    .await
                    .map_err(|error| anyhow!(error.to_string()))?;

                debug!(target: LOG_CONSENSUS, %txid,  "Transaction accepted");
                tx.insert(&ACCEPTED_TRANSACTION, &txid, &modules_ids);

                Ok(())
            }
        }
    }

    /// Returns the number of sessions already saved in the database. This count
    /// **does not** include the currently running session.
    async fn get_finished_session_count(&self) -> u64 {
        get_finished_session_count_static(&self.db.begin_read().await).await
    }
}

pub async fn get_finished_session_count_static(tx: &ReadTransaction) -> u64 {
    tx.iter(&SIGNED_SESSION_OUTCOME)
        .into_iter()
        .next_back()
        .map_or(0, |(k, _)| k + 1)
}
