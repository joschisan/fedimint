#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::too_many_lines)]

pub use picomint_core::mint as common;

mod api;
mod client_db;
mod ecash;
mod events;
pub mod issuance;
mod issuance_sm;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::api::FederationApi;
use crate::module::ClientContext;
use crate::transaction::{Input, Output, TransactionBuilder};
use crate::transaction::{Transaction, TxSubmissionSmContext, TxSubmissionStateMachine};
use anyhow::{Context as _, bail};
use bitcoin_hashes::sha256;
use client_db::{NOTE, RECEIVE_OPERATION, RECOVERY_STATE, RecoveryState};
pub use events::*;
use futures::StreamExt;
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::mint::config::{MintConfigConsensus, client_denominations};
use picomint_core::mint::{Denomination, MintInput, Note, RecoveryItem};
use picomint_core::secp256k1::rand::{Rng, thread_rng};
use picomint_core::secp256k1::{Keypair, PublicKey};
use picomint_core::task::TaskGroup;
use picomint_core::{Amount, PeerId, TransactionId, wire};
use crate::secret::Secret;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::Database;
use picomint_redb::WriteTxRef;
use rand::seq::IteratorRandom;
use tbs::AggregatePublicKey;
use thiserror::Error;

pub use self::ecash::ECash;
use self::issuance::NoteIssuanceRequest;
use self::issuance_sm::IssuanceStateMachine;

const TARGET_PER_DENOMINATION: usize = 3;
const SLICE_SIZE: u64 = 10000;
const PARALLEL_HASH_REQUESTS: usize = 10;
const PARALLEL_SLICE_REQUESTS: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Encodable, Decodable)]
pub struct SpendableNote {
    pub denomination: Denomination,
    pub keypair: Keypair,
    pub signature: tbs::Signature,
}

picomint_redb::consensus_key!(SpendableNote);

impl SpendableNote {
    pub fn amount(&self) -> Amount {
        self.denomination.amount()
    }
}

impl SpendableNote {
    fn nonce(&self) -> PublicKey {
        self.keypair.public_key()
    }

    fn note(&self) -> Note {
        Note {
            denomination: self.denomination,
            nonce: self.nonce(),
            signature: self.signature,
        }
    }
}

impl MintClientModule {
    pub async fn recover(
        db: &Database,
        api: &FederationApi,
        module_api: &FederationApi,
        module_root_secret: &Secret,
    ) -> anyhow::Result<()> {
        let mut state = if let Some(state) = db.begin_read().await.get(&RECOVERY_STATE, &()) {
            state
        } else {
            RecoveryState {
                next_index: 0,
                total_items: module_api.recovery_count().await?,
                requests: BTreeMap::new(),
                nonces: BTreeSet::new(),
            }
        };

        if state.next_index == state.total_items {
            return Ok(());
        }

        let peer_selector = PeerSelector::new(api.all_peers().clone());

        let mut recovery_stream = futures::stream::iter(
            (state.next_index..state.total_items).step_by(SLICE_SIZE as usize),
        )
        .map(|start| {
            let api = module_api.clone();
            let end = std::cmp::min(start + SLICE_SIZE, state.total_items);

            async move { (start, end, api.recovery_slice_hash(start, end).await) }
        })
        .buffered(PARALLEL_HASH_REQUESTS)
        .map(|(start, end, hash)| {
            download_slice_with_hash(module_api.clone(), peer_selector.clone(), start, end, hash)
        })
        .buffered(PARALLEL_SLICE_REQUESTS);

        let tweak_filter = issuance::tweak_filter(module_root_secret);

        loop {
            let items = recovery_stream
                .next()
                .await
                .context("Recovery stream finished before recovery is complete")?;

            for item in &items {
                match item {
                    RecoveryItem::Output {
                        denomination,
                        nonce_hash,
                        tweak,
                    } => {
                        if !issuance::check_tweak(*tweak, tweak_filter) {
                            continue;
                        }
                        let output_secret =
                            issuance::output_secret(*denomination, *tweak, module_root_secret);

                        if !issuance::check_nonce(&output_secret, *nonce_hash) {
                            continue;
                        }

                        let computed_nonce_hash = issuance::nonce(&output_secret).consensus_hash();

                        // Ignore possible duplicate nonces
                        if !state.nonces.insert(computed_nonce_hash) {
                            continue;
                        }

                        state.requests.insert(
                            computed_nonce_hash,
                            NoteIssuanceRequest::new(*denomination, *tweak, module_root_secret),
                        );
                    }
                    RecoveryItem::Input { nonce_hash } => {
                        state.requests.remove(nonce_hash);
                        state.nonces.remove(nonce_hash);
                    }
                }
            }

            state.next_index += items.len() as u64;

            let dbtx = db.begin_write().await;
            let tx = dbtx.as_ref();

            tx.insert(&RECOVERY_STATE, &(), &state);

            if state.next_index == state.total_items {
                // Persist the recovery-bootstrapped Output SM under the
                // executor's table. When the module is constructed, the
                // executor picks this up via `get_active_states` and drives it.
                let sm = IssuanceStateMachine {
                    operation_id: OperationId::new_random(),
                    spendable_notes: vec![],
                    txid: None,
                    issuance_requests: state.requests.into_values().collect(),
                };

                crate::executor::ModuleExecutor::add_state_machine_unstarted(&tx, sm).await;

                dbtx.commit().await;

                return Ok(());
            }

            dbtx.commit().await;

            tracing::info!(
                target: picomint_logging::LOG_CLIENT,
                next_index = state.next_index,
                total_items = state.total_items,
                "Mint recovery progress"
            );
        }
    }

    pub async fn new(
        federation_id: FederationId,
        cfg: MintConfigConsensus,
        context: ClientContext,
        module_root_secret: &Secret,
        task_group: &TaskGroup,
    ) -> anyhow::Result<MintClientModule> {
        let (tweak_sender, tweak_receiver) = async_channel::bounded(50);

        let filter = issuance::tweak_filter(module_root_secret);

        tokio::task::spawn_blocking(move || {
            loop {
                let tweak: [u8; 16] = thread_rng().r#gen();

                if !issuance::check_tweak(tweak, filter) {
                    continue;
                }

                if tweak_sender.send_blocking(tweak).is_err() {
                    return;
                }
            }
        });

        let sm_context = MintSmContext {
            client_ctx: context.clone(),
            tbs_agg_pks: cfg.tbs_agg_pks.clone(),
            tbs_pks: cfg.tbs_pks.clone(),
        };

        let issuance_executor = crate::executor::ModuleExecutor::new(
            context.db().clone(),
            sm_context,
            task_group.clone(),
        )
        .await;

        let tx_submission_executor = crate::executor::ModuleExecutor::new(
            context.db().clone(),
            TxSubmissionSmContext {
                api: context.global_api(),
            },
            task_group.clone(),
        )
        .await;

        Ok(MintClientModule {
            federation_id,
            cfg,
            root_secret: module_root_secret.clone(),
            client_ctx: context,
            tweak_receiver,
            tx_submission_executor,
            issuance_executor,
        })
    }
}

#[derive(Debug)]
pub struct MintClientModule {
    federation_id: FederationId,
    cfg: MintConfigConsensus,
    root_secret: Secret,
    client_ctx: ClientContext,
    tweak_receiver: async_channel::Receiver<[u8; 16]>,
    tx_submission_executor: crate::executor::ModuleExecutor<TxSubmissionStateMachine>,
    issuance_executor: crate::executor::ModuleExecutor<IssuanceStateMachine>,
}

/// Context handed to per-SM executors. Keeps the `ClientContext` handle
/// plus the immutable config data SMs need.
#[derive(Debug, Clone)]
pub struct MintSmContext {
    pub client_ctx: ClientContext,
    pub tbs_agg_pks: BTreeMap<Denomination, AggregatePublicKey>,
    pub tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, tbs::PublicKeyShare>>,
}

impl MintClientModule {
    pub fn input_fee(&self) -> Amount {
        self.cfg.input_fee
    }

    pub fn output_fee(&self) -> Amount {
        self.cfg.output_fee
    }

    /// Balance the builder against mint's wallet (pulling funding notes when
    /// underfunded, generating change outputs when overfunded), sign and
    /// submit the resulting transaction, and spawn the
    /// `IssuanceStateMachine` that tracks the balance-side notes/requests
    /// (if any).
    pub async fn finalize_and_submit_transaction(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        mut builder: TransactionBuilder,
    ) -> anyhow::Result<TransactionId> {
        let (spendable_notes, issuance_requests) = self.balance(dbtx, &mut builder).await?;

        let txid = self.submit(dbtx, operation_id, builder).await?;

        if !spendable_notes.is_empty() || !issuance_requests.is_empty() {
            let sm = IssuanceStateMachine {
                operation_id,
                spendable_notes,
                txid: Some(txid),
                issuance_requests,
            };
            self.issuance_executor.add_state_machine_dbtx(dbtx, sm).await;
        }

        Ok(txid)
    }

    /// Mint-side transaction balancing. Pulls funding notes from the wallet
    /// when the builder is underfunded, then absorbs any excess as change
    /// outputs. Sub-denomination dust below `smallest_denom + output_fee` is
    /// left as implicit federation revenue.
    async fn balance(
        &self,
        dbtx: &WriteTxRef<'_>,
        builder: &mut TransactionBuilder,
    ) -> anyhow::Result<(Vec<SpendableNote>, Vec<NoteIssuanceRequest>)> {
        let mut spendable_notes = self
            .select_funding_input(dbtx, builder.deficit())
            .await
            .context("Insufficient funds")?;

        // Sort by denomination to minimize information leaked about
        // which notes the wallet held.
        spendable_notes.sort_by_key(|note| note.denomination);

        for note in &spendable_notes {
            self.remove_spendable_note(dbtx, note).await;
            builder.add_input(Input {
                input: wire::Input::Mint(MintInput { note: note.note() }),
                keypair: note.keypair,
                amount: note.amount(),
                fee: self.cfg.input_fee,
            });
        }

        assert_eq!(builder.deficit(), Amount::ZERO);

        let mut denoms = self
            .select_output_denominations(dbtx, self.cfg.output_fee, builder.excess_input())
            .await;

        // Sort to minimize information leaked about the change shape.
        denoms.sort();

        let issuance_requests: Vec<NoteIssuanceRequest> = futures::stream::iter(denoms)
            .zip(self.tweak_receiver.clone())
            .map(|(d, tweak)| NoteIssuanceRequest::new(d, tweak, &self.root_secret))
            .collect()
            .await;

        for request in &issuance_requests {
            builder.add_output(Output {
                output: wire::Output::Mint(request.output()),
                amount: request.denomination.amount(),
                fee: self.cfg.output_fee,
            });
        }

        assert_eq!(builder.deficit(), Amount::ZERO);

        Ok((spendable_notes, issuance_requests))
    }

    /// Sign the builder, size-check the encoded transaction, and spawn the
    /// `TxSubmissionStateMachine`.
    async fn submit(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        builder: TransactionBuilder,
    ) -> anyhow::Result<TransactionId> {
        let transaction = builder.build();

        if transaction.consensus_encode_to_vec().len() > Transaction::MAX_TX_SIZE {
            bail!("The generated transaction is too large.");
        }

        let txid = transaction.tx_hash();

        let sm = TxSubmissionStateMachine {
            operation_id,
            transaction,
        };

        self.tx_submission_executor
            .add_state_machine_dbtx(dbtx, sm)
            .await;

        Ok(txid)
    }

    pub async fn get_balance(&self, dbtx: &WriteTxRef<'_>) -> Amount {
        self.get_count_by_denomination_dbtx(dbtx)
            .await
            .into_iter()
            .map(|(denomination, count)| denomination.amount().mul_u64(count))
            .sum()
    }

    pub fn balance_notify(&self) -> Arc<tokio::sync::Notify> {
        self.client_ctx.db().notify_for_table(&NOTE)
    }

    async fn select_funding_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        mut excess_output: Amount,
    ) -> Option<Vec<SpendableNote>> {
        let mut selected_notes = Vec::new();
        let mut target_notes = Vec::new();
        let mut excess_notes = Vec::new();

        let all_notes: Vec<SpendableNote> = dbtx
            .iter(&NOTE)
            .into_iter()
            .map(|(note, ())| note)
            .collect();

        for amount in client_denominations().rev() {
            let notes_amount: Vec<SpendableNote> = all_notes
                .iter()
                .filter(|note| note.denomination == amount)
                .cloned()
                .collect();

            target_notes.extend(notes_amount.iter().take(TARGET_PER_DENOMINATION).cloned());

            if notes_amount.len() > 2 * TARGET_PER_DENOMINATION {
                for note in notes_amount.into_iter().skip(TARGET_PER_DENOMINATION) {
                    let note_value = note
                        .amount()
                        .checked_sub(self.cfg.input_fee)
                        .expect("All our notes are economical");

                    excess_output = excess_output.saturating_sub(note_value);

                    selected_notes.push(note);
                }
            } else {
                excess_notes.extend(notes_amount.into_iter().skip(TARGET_PER_DENOMINATION));
            }
        }

        if excess_output == Amount::ZERO {
            return Some(selected_notes);
        }

        for note in excess_notes.into_iter().chain(target_notes) {
            let note_value = note
                .amount()
                .checked_sub(self.cfg.input_fee)
                .expect("All our notes are economical");

            excess_output = excess_output.saturating_sub(note_value);

            selected_notes.push(note);

            if excess_output == Amount::ZERO {
                return Some(selected_notes);
            }
        }

        None
    }

    async fn select_output_denominations(
        &self,
        dbtx: &WriteTxRef<'_>,
        output_fee: Amount,
        mut excess_input: Amount,
    ) -> Vec<Denomination> {
        let n_denominations = self.get_count_by_denomination_dbtx(dbtx).await;

        let mut output_denominations = Vec::new();

        // Rebalance per-tier reserves up to TARGET_PER_DENOMINATION, smallest->largest.
        for d in client_denominations() {
            let n_missing = TARGET_PER_DENOMINATION
                .saturating_sub(n_denominations.get(&d).copied().unwrap_or(0) as usize);

            for _ in 0..n_missing {
                match excess_input.checked_sub(d.amount() + output_fee) {
                    Some(remaining) => {
                        excess_input = remaining;
                        output_denominations.push(d);
                    }
                    None => break,
                }
            }
        }

        // Absorb remaining excess as change, largest->smallest.
        for d in client_denominations().rev() {
            for _ in 0.. {
                match excess_input.checked_sub(d.amount() + output_fee) {
                    Some(remaining) => {
                        excess_input = remaining;
                        output_denominations.push(d);
                    }
                    None => break,
                }
            }
        }

        output_denominations
    }
}

impl MintClientModule {
    /// Count the `ECash` notes in the client's database by denomination.
    pub async fn get_count_by_denomination(&self) -> BTreeMap<Denomination, u64> {
        let dbtx = self.client_ctx.db().begin_write().await;

        self.get_count_by_denomination_dbtx(&dbtx.as_ref()).await
    }

    async fn get_count_by_denomination_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
    ) -> BTreeMap<Denomination, u64> {
        let mut acc = BTreeMap::new();
        for (note, ()) in dbtx.iter(&NOTE) {
            acc.entry(note.denomination)
                .and_modify(|count| *count += 1)
                .or_insert(1);
        }
        acc
    }

    /// Send `ECash` for the given amount. The
    /// amount will be rounded up to a multiple of 512 msats which is the
    /// smallest denomination used throughout the client. If the rounded
    /// amount cannot be covered with the ecash notes in the client's
    /// database the client will create a transaction to reissue the
    /// required denominations. It is safe to cancel the send method call
    /// before the reissue is complete in which case the reissued notes are
    /// returned to the regular balance. To cancel a successful ecash send
    /// simply receive it yourself.
    pub async fn send(&self, amount: Amount) -> Result<ECash, SendECashError> {
        let amount = round_to_multiple(amount, client_denominations().next().unwrap().amount());

        let dbtx = self.client_ctx.db().begin_write().await;

        let ecash = self
            .send_ecash_dbtx(&dbtx.as_ref(), amount)
            .await
            .expect("Infallible");

        dbtx.commit().await;

        if let Some(ecash) = ecash {
            return Ok(ecash);
        }

        self.client_ctx
            .global_api()
            .liveness()
            .await
            .map_err(|_| SendECashError::Offline)?;

        let operation_id = OperationId::new_random();
        let target_denominations = represent_amount(amount);

        // Build target issuance requests up-front. Their outputs go into the
        // builder first; the balance loop then pulls funding from the wallet
        // and appends change outputs. We extend `issuance_requests` with the
        // change requests after balance so the order matches the transaction's
        // outputs and a single `IssuanceStateMachine` can process both.
        let mut issuance_requests: Vec<NoteIssuanceRequest> =
            futures::stream::iter(target_denominations)
                .zip(self.tweak_receiver.clone())
                .map(|(d, tweak)| NoteIssuanceRequest::new(d, tweak, &self.root_secret))
                .collect()
                .await;

        let mut builder = TransactionBuilder::new();
        for request in &issuance_requests {
            builder.add_output(Output {
                output: wire::Output::Mint(request.output()),
                amount: request.denomination.amount(),
                fee: self.cfg.output_fee,
            });
        }

        let dbtx = self.client_ctx.db().begin_write().await;
        let tx = dbtx.as_ref();

        let (funding_notes, change_requests) = self
            .balance(&tx, &mut builder)
            .await
            .map_err(|_| SendECashError::InsufficientBalance)?;

        let txid = self
            .submit(&tx, operation_id, builder)
            .await
            .map_err(|_| SendECashError::Failure)?;

        issuance_requests.extend(change_requests);

        let sm = IssuanceStateMachine {
            operation_id,
            spendable_notes: funding_notes,
            txid: Some(txid),
            issuance_requests,
        };

        self.issuance_executor.add_state_machine_dbtx(&tx, sm).await;

        self.client_ctx
            .log_event(&tx, operation_id, ReissueEvent { txid })
            .await;

        dbtx.commit().await;

        self.client_ctx
            .subscribe_operation_events_typed::<events::IssuanceComplete>(operation_id)
            .next()
            .await;

        Box::pin(self.send(amount)).await
    }

    async fn send_ecash_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        mut remaining_amount: Amount,
    ) -> Result<Option<ECash>, Infallible> {
        let mut sorted: Vec<SpendableNote> = dbtx
            .iter(&NOTE)
            .into_iter()
            .map(|(note, ())| note)
            .collect();
        sorted.sort_by(|a, b| b.denomination.cmp(&a.denomination));

        let mut notes = vec![];

        for spendable_note in sorted {
            remaining_amount = match remaining_amount.checked_sub(spendable_note.amount()) {
                Some(amount) => amount,
                None => continue,
            };

            notes.push(spendable_note);
        }

        if remaining_amount != Amount::ZERO {
            return Ok(None);
        }

        for spendable_note in &notes {
            self.remove_spendable_note(dbtx, spendable_note).await;
        }

        let ecash = ECash::new(self.federation_id, notes);
        let amount = ecash.amount();
        let operation_id = OperationId::new_random();

        self.client_ctx
            .log_event(
                dbtx,
                operation_id,
                SendEvent {
                    amount,
                    ecash: picomint_base32::encode(&ecash),
                },
            )
            .await;

        Ok(Some(ecash))
    }

    /// Receive the `ECash` by reissuing the notes. This method is idempotent
    /// via the deterministic [`OperationId`] derived from the ecash bytes.
    pub async fn receive(&self, ecash: ECash) -> Result<OperationId, ReceiveECashError> {
        let operation_id = OperationId::from_encodable(&ecash);

        if ecash.mint() != Some(self.federation_id) {
            return Err(ReceiveECashError::WrongFederation);
        }

        if ecash
            .notes()
            .iter()
            .any(|note| note.amount() <= self.cfg.input_fee)
        {
            return Err(ReceiveECashError::UneconomicalDenomination);
        }

        let mut tx_builder = TransactionBuilder::new();
        for note in ecash.notes() {
            tx_builder.add_input(Input {
                input: wire::Input::Mint(MintInput { note: note.note() }),
                keypair: note.keypair,
                amount: note.amount(),
                fee: self.cfg.input_fee,
            });
        }

        let dbtx = self.client_ctx.db().begin_write().await;

        if dbtx
            .as_ref()
            .insert(&RECEIVE_OPERATION, &operation_id, &())
            .is_some()
        {
            return Ok(operation_id);
        }

        let txid = self
            .finalize_and_submit_transaction(&dbtx.as_ref(), operation_id, tx_builder)
            .await
            .map_err(|_| ReceiveECashError::InsufficientFunds)?;

        let event = ReceiveEvent {
            txid,
            amount: ecash.amount(),
        };

        self.client_ctx
            .log_event(&dbtx.as_ref(), operation_id, event)
            .await;

        dbtx.commit().await;

        Ok(operation_id)
    }

    async fn remove_spendable_note(&self, dbtx: &WriteTxRef<'_>, spendable_note: &SpendableNote) {
        dbtx.remove(&NOTE, spendable_note)
            .expect("Must delete existing spendable note");
    }
}

#[derive(Clone)]
struct PeerSelector {
    latency: Arc<RwLock<BTreeMap<PeerId, Duration>>>,
}

impl PeerSelector {
    fn new(peers: BTreeSet<PeerId>) -> Self {
        let latency = peers
            .into_iter()
            .map(|peer| (peer, Duration::ZERO))
            .collect();

        Self {
            latency: Arc::new(RwLock::new(latency)),
        }
    }

    /// Pick 2 peers at random, return the one with lower latency
    fn choose_peer(&self) -> PeerId {
        let latency = self.latency.read().unwrap();

        let peer_a = latency.iter().choose(&mut thread_rng()).unwrap();
        let peer_b = latency.iter().choose(&mut thread_rng()).unwrap();

        if peer_a.1 <= peer_b.1 {
            *peer_a.0
        } else {
            *peer_b.0
        }
    }

    // Update with exponential moving average (α = 0.1)
    fn report(&self, peer: PeerId, duration: Duration) {
        self.latency
            .write()
            .unwrap()
            .entry(peer)
            .and_modify(|latency| *latency = *latency * 9 / 10 + duration * 1 / 10)
            .or_insert(duration);
    }

    fn remove(&self, peer: PeerId) {
        self.latency.write().unwrap().remove(&peer);
    }
}

/// Download a slice with hash verification and peer selection
async fn download_slice_with_hash(
    module_api: FederationApi,
    peer_selector: PeerSelector,
    start: u64,
    end: u64,
    expected_hash: sha256::Hash,
) -> Vec<RecoveryItem> {
    const TIMEOUT: Duration = Duration::from_secs(30);

    loop {
        let peer = peer_selector.choose_peer();
        let start_time = picomint_core::time::now();

        if let Ok(data) = module_api.recovery_slice(peer, TIMEOUT, start, end).await {
            let elapsed = picomint_core::time::now()
                .duration_since(start_time)
                .unwrap_or_default();

            peer_selector.report(peer, elapsed);

            if data.consensus_hash::<sha256::Hash>() == expected_hash {
                return data;
            }

            peer_selector.remove(peer);
        } else {
            peer_selector.report(peer, TIMEOUT);
        }
    }
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum SendECashError {
    #[error("We need to reissue notes but the client is offline")]
    Offline,
    #[error("The clients balance is insufficient")]
    InsufficientBalance,
    #[error("A non-recoverable error has occurred")]
    Failure,
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum ReceiveECashError {
    #[error("The ECash is from a different federation")]
    WrongFederation,
    #[error("ECash contains an uneconomical denomination")]
    UneconomicalDenomination,
    #[error("Receiving ecash requires additional funds")]
    InsufficientFunds,
}

fn round_to_multiple(amount: Amount, min_denomiation: Amount) -> Amount {
    Amount::from_msats(amount.msats.next_multiple_of(min_denomiation.msats))
}

fn represent_amount(mut remaining_amount: Amount) -> Vec<Denomination> {
    let mut denominations = Vec::new();

    // Add denominations with a greedy algorithm
    for denomination in client_denominations().rev() {
        let n_add = remaining_amount / denomination.amount();

        denominations.extend(std::iter::repeat_n(denomination, n_add as usize));

        remaining_amount -= n_add * denomination.amount();
    }

    denominations
}
