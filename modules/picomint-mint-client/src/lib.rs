#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::too_many_lines)]

pub use picomint_mint_common as common;

mod api;
mod client_db;
mod ecash;
mod events;
mod input;
pub mod issuance;
mod output;
mod receive;

use std::collections::{BTreeMap, BTreeSet};
use std::convert::Infallible;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use anyhow::Context as _;
use bitcoin_hashes::sha256;
use client_db::{RecoveryState, NOTE, RECEIVE_OPERATION, RECOVERY_STATE};
pub use events::*;
use futures::StreamExt;
use picomint_api_client::api::FederationApi;
use picomint_client_module::module::init::{
    ClientModuleInit, ClientModuleInitArgs, ClientModuleRecoverArgs,
};
use picomint_client_module::module::recovery::RecoveryProgress;
use picomint_client_module::module::{ClientContext, ClientModule, IdxRange, OutPointRange};
use picomint_client_module::transaction::{
    ClientInput, ClientInputBundle, ClientOutput, ClientOutputBundle, TransactionBuilder,
};
use picomint_core::base32::{self, PICOMINT_PREFIX};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::module::{ModuleCommon, ModuleInit};
use picomint_core::secp256k1::rand::{thread_rng, Rng};
use picomint_core::secp256k1::{Keypair, PublicKey};
use picomint_core::util::BoxStream;
use picomint_core::{apply, async_trait_maybe_send, Amount, PeerId};
use picomint_derive_secret::DerivableSecret;
use picomint_mint_common::config::{client_denominations, MintConfigConsensus};
use picomint_mint_common::{
    Denomination, MintCommonInit, MintInput, MintModuleTypes, MintOutput, Note, RecoveryItem,
};
use picomint_redb::WriteTxRef;
use rand::seq::IteratorRandom;
use tbs::AggregatePublicKey;
use thiserror::Error;

use crate::api::MintV2ModuleApi;
pub use crate::ecash::ECash;
use crate::input::{InputSMCommon, InputSMState, InputStateMachine};
use crate::issuance::NoteIssuanceRequest;
use crate::output::{MintOutputStateMachine, OutputSMCommon, OutputSMState};
use crate::receive::ReceiveStateMachine;

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

picomint_core::consensus_key!(SpendableNote);

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

#[derive(Debug, Clone)]
pub struct MintClientInit;

impl ModuleInit for MintClientInit {
    type Common = MintCommonInit;
}

#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for MintClientInit {
    type Module = MintClientModule;

    async fn recover(&self, args: &ClientModuleRecoverArgs<Self>) -> anyhow::Result<()> {
        let mut state = if let Some(state) = args.db().begin_read().await.get(&RECOVERY_STATE, &())
        {
            state
        } else {
            RecoveryState {
                next_index: 0,
                total_items: args.module_api().fetch_recovery_count().await?,
                requests: BTreeMap::new(),
                nonces: BTreeSet::new(),
            }
        };

        if state.next_index == state.total_items {
            return Ok(());
        }

        let peer_selector = PeerSelector::new(args.api().all_peers().clone());

        let mut recovery_stream = futures::stream::iter(
            (state.next_index..state.total_items).step_by(SLICE_SIZE as usize),
        )
        .map(|start| {
            let api = args.module_api().clone();
            let end = std::cmp::min(start + SLICE_SIZE, state.total_items);

            async move { (start, end, api.fetch_recovery_slice_hash(start, end).await) }
        })
        .buffered(PARALLEL_HASH_REQUESTS)
        .map(|(start, end, hash)| {
            download_slice_with_hash(
                args.module_api().clone(),
                peer_selector.clone(),
                start,
                end,
                hash,
            )
        })
        .buffered(PARALLEL_SLICE_REQUESTS);

        let tweak_filter = issuance::tweak_filter(args.module_root_secret());

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
                        let output_secret = issuance::output_secret(
                            *denomination,
                            *tweak,
                            args.module_root_secret(),
                        );

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
                            NoteIssuanceRequest::new(
                                *denomination,
                                *tweak,
                                args.module_root_secret(),
                            ),
                        );
                    }
                    RecoveryItem::Input { nonce_hash } => {
                        state.requests.remove(nonce_hash);
                        state.nonces.remove(nonce_hash);
                    }
                }
            }

            state.next_index += items.len() as u64;

            let dbtx = args.db().begin_write().await;
            let tx = dbtx.as_ref();

            tx.insert(&RECOVERY_STATE, &(), &state);

            if state.next_index == state.total_items {
                // Persist the recovery-bootstrapped Output SM under the
                // executor's table. When the module loads and
                // `output_executor.start()` runs, it picks this up via
                // `get_active_states` and drives it.
                let sm = MintOutputStateMachine {
                    common: OutputSMCommon {
                        operation_id: OperationId::new_random(),
                        range: None,
                        issuance_requests: state.requests.into_values().collect(),
                    },
                    state: OutputSMState::Pending,
                };

                picomint_client_module::executor::ModuleExecutor::add_state_machine_unstarted(
                    &tx, sm,
                )
                .await;

                dbtx.commit().await;

                return Ok(());
            }

            dbtx.commit().await;

            args.update_recovery_progress(RecoveryProgress {
                complete: state.next_index.try_into().unwrap_or(u32::MAX),
                total: state.total_items.try_into().unwrap_or(u32::MAX),
            });
        }
    }

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        let (tweak_sender, tweak_receiver) = async_channel::bounded(50);

        let filter = issuance::tweak_filter(args.module_root_secret());

        tokio::task::spawn_blocking(move || loop {
            let tweak: [u8; 16] = thread_rng().r#gen();

            if !issuance::check_tweak(tweak, filter) {
                continue;
            }

            if tweak_sender.send_blocking(tweak).is_err() {
                return;
            }
        });

        let cfg: MintConfigConsensus = args.cfg().clone();
        let client_ctx = args.context();
        let balance_update_sender = tokio::sync::watch::channel(()).0;

        let sm_context = MintSmContext {
            client_ctx: client_ctx.clone(),
            tbs_agg_pks: cfg.tbs_agg_pks.clone(),
            tbs_pks: cfg.tbs_pks.clone(),
            balance_update_sender: balance_update_sender.clone(),
        };

        let task_group = args.task_group().clone();

        let input_executor = picomint_client_module::executor::ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context.clone(),
            task_group.clone(),
        );
        let output_executor = picomint_client_module::executor::ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context.clone(),
            task_group.clone(),
        );
        let receive_executor = picomint_client_module::executor::ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context,
            task_group,
        );

        Ok(MintClientModule {
            federation_id: *args.federation_id(),
            cfg,
            root_secret: args.module_root_secret().clone(),
            client_ctx,
            balance_update_sender,
            tweak_receiver,
            input_executor,
            output_executor,
            receive_executor,
        })
    }
}

#[derive(Debug)]
pub struct MintClientModule {
    federation_id: FederationId,
    cfg: MintConfigConsensus,
    root_secret: DerivableSecret,
    client_ctx: ClientContext<Self>,
    balance_update_sender: tokio::sync::watch::Sender<()>,
    tweak_receiver: async_channel::Receiver<[u8; 16]>,
    input_executor: picomint_client_module::executor::ModuleExecutor<InputStateMachine>,
    output_executor: picomint_client_module::executor::ModuleExecutor<MintOutputStateMachine>,
    receive_executor: picomint_client_module::executor::ModuleExecutor<ReceiveStateMachine>,
}

/// Context handed to per-SM executors. Keeps the `ClientContext` handle
/// plus the immutable config data SMs need.
#[derive(Debug, Clone)]
pub struct MintSmContext {
    pub client_ctx: ClientContext<MintClientModule>,
    pub tbs_agg_pks: BTreeMap<Denomination, AggregatePublicKey>,
    pub tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, tbs::PublicKeyShare>>,
    pub balance_update_sender: tokio::sync::watch::Sender<()>,
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for MintClientModule {
    type Init = MintClientInit;
    type Common = MintModuleTypes;

    async fn start(&self) {
        self.input_executor.start().await;
        self.output_executor.start().await;
        self.receive_executor.start().await;
    }

    fn input_fee(
        &self,
        _amount: Amount,
        _input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amount> {
        Some(self.cfg.input_fee)
    }

    fn output_fee(
        &self,
        _amount: Amount,
        _output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amount> {
        Some(self.cfg.output_fee)
    }

    fn supports_being_primary(&self) -> bool {
        true
    }

    async fn create_final_inputs_and_outputs(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        mut input_amount: Amount,
        mut output_amount: Amount,
    ) -> anyhow::Result<picomint_client_module::module::FinalContribution<MintInput, MintOutput>>
    {
        let funding_notes = self
            .select_funding_input(dbtx, output_amount.saturating_sub(input_amount))
            .await
            .context("Insufficient funds")?;

        for note in &funding_notes {
            self.remove_spendable_note(dbtx, note).await;
        }

        input_amount += funding_notes.iter().map(SpendableNote::amount).sum();

        output_amount += self.cfg.input_fee * funding_notes.len() as u64;

        assert!(output_amount <= input_amount);

        let (input_notes, output_amounts) = self
            .rebalance(dbtx, self.cfg.output_fee, input_amount - output_amount)
            .await;

        for note in &input_notes {
            self.remove_spendable_note(dbtx, note).await;
        }

        input_amount += input_notes.iter().map(SpendableNote::amount).sum();

        output_amount += self.cfg.input_fee * input_notes.len() as u64;

        output_amount += output_amounts
            .iter()
            .map(|denomination| denomination.amount() + self.cfg.output_fee)
            .sum();

        assert!(output_amount <= input_amount);

        let mut spendable_notes = funding_notes
            .into_iter()
            .chain(input_notes)
            .collect::<Vec<SpendableNote>>();

        // We sort the notes by denomination to minimize the leaked information.
        spendable_notes.sort_by_key(|note| note.denomination);

        let inputs = build_inputs(&spendable_notes);

        let mut denominations = represent_amount_with_fees(
            input_amount.saturating_sub(output_amount),
            self.cfg.output_fee,
        )
        .into_iter()
        .chain(output_amounts)
        .collect::<Vec<Denomination>>();

        // We sort the amounts to minimize the leaked information.
        denominations.sort();

        let (issuance_requests, outputs) = self.build_issuance(denominations).await;

        let sender = self.balance_update_sender.clone();
        dbtx.on_commit(move || sender.send_replace(()));

        let input_executor = self.input_executor.clone();
        let output_executor = self.output_executor.clone();

        let spawn_sms: picomint_client_module::module::SpawnSms =
            Box::new(move |dbtx, txid, _primary_in_range, primary_out_range| {
                Box::pin(async move {
                    if !spendable_notes.is_empty() {
                        input_executor
                            .add_state_machine_dbtx(
                                dbtx,
                                InputStateMachine {
                                    common: InputSMCommon {
                                        operation_id,
                                        txid,
                                        spendable_notes,
                                    },
                                    state: InputSMState::Pending,
                                },
                            )
                            .await;
                    }

                    if !issuance_requests.is_empty() {
                        output_executor
                            .add_state_machine_dbtx(
                                dbtx,
                                MintOutputStateMachine {
                                    common: OutputSMCommon {
                                        operation_id,
                                        range: Some(OutPointRange::new(txid, primary_out_range)),
                                        issuance_requests,
                                    },
                                    state: OutputSMState::Pending,
                                },
                            )
                            .await;
                    }
                })
            });

        Ok(picomint_client_module::module::FinalContribution {
            inputs,
            outputs,
            spawn_sms,
        })
    }

    async fn get_balance(&self, dbtx: &WriteTxRef<'_>) -> Amount {
        self.get_count_by_denomination_dbtx(dbtx)
            .await
            .into_iter()
            .map(|(denomination, count)| denomination.amount().mul_u64(count))
            .sum()
    }

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        Box::pin(tokio_stream::wrappers::WatchStream::new(
            self.balance_update_sender.subscribe(),
        ))
    }
}

impl MintClientModule {
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

    async fn rebalance(
        &self,
        dbtx: &WriteTxRef<'_>,
        output_fee: Amount,
        mut excess_input: Amount,
    ) -> (Vec<SpendableNote>, Vec<Denomination>) {
        let n_denominations = self.get_count_by_denomination_dbtx(dbtx).await;

        let mut notes: Vec<SpendableNote> = dbtx
            .iter(&NOTE)
            .into_iter()
            .map(|(note, ())| note)
            .collect();
        notes.sort_by(|a, b| b.denomination.cmp(&a.denomination));
        let mut notes = notes.into_iter();

        let mut input_notes = Vec::new();
        let mut output_denominations = Vec::new();

        for d in client_denominations() {
            let n_denomination = n_denominations.get(&d).copied().unwrap_or(0);

            let n_missing = TARGET_PER_DENOMINATION.saturating_sub(n_denomination as usize);

            for _ in 0..n_missing {
                match excess_input.checked_sub(d.amount() + output_fee) {
                    Some(remaining_excess) => excess_input = remaining_excess,
                    None => match notes.next() {
                        Some(note) => {
                            if note.amount() <= d.amount() + output_fee {
                                break;
                            }

                            excess_input += note.amount() - (d.amount() + output_fee);

                            input_notes.push(note);
                        }
                        None => break,
                    },
                }

                output_denominations.push(d);
            }
        }

        (input_notes, output_denominations)
    }
}

fn build_inputs(notes: &[SpendableNote]) -> Vec<ClientInput<MintInput>> {
    notes
        .iter()
        .map(|spendable_note| ClientInput {
            input: MintInput { note: spendable_note.note() },
            keys: vec![spendable_note.keypair],
            amount: spendable_note.amount(),
        })
        .collect()
}

impl MintClientModule {
    async fn build_issuance(
        &self,
        requested_denominations: Vec<Denomination>,
    ) -> (Vec<NoteIssuanceRequest>, Vec<ClientOutput<MintOutput>>) {
        let issuance_requests = futures::stream::iter(requested_denominations)
            .zip(self.tweak_receiver.clone())
            .map(|(d, tweak)| NoteIssuanceRequest::new(d, tweak, &self.root_secret))
            .collect::<Vec<NoteIssuanceRequest>>()
            .await;

        let outputs = issuance_requests
            .iter()
            .map(|request| ClientOutput {
                output: request.output(),
                amount: request.denomination.amount(),
            })
            .collect();

        (issuance_requests, outputs)
    }

    /// Count the `ECash` notes in the client's database by denomination.
    pub async fn get_count_by_denomination(&self) -> BTreeMap<Denomination, u64> {
        let dbtx = self.client_ctx.module_db().begin_write().await;
        let counts = self.get_count_by_denomination_dbtx(&dbtx.as_ref()).await;
        drop(dbtx);
        counts
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

        let module_db = self.client_ctx.module_db();
        let maybe_ecash = {
            let dbtx = module_db.begin_write().await;
            let ecash = self
                .send_ecash_dbtx(&dbtx.as_ref(), amount)
                .await
                .expect("Infallible");
            dbtx.commit().await;
            ecash
        };
        if let Some(ecash) = maybe_ecash {
            return Ok(ecash);
        }

        self.client_ctx
            .global_api()
            .liveness()
            .await
            .map_err(|_| SendECashError::Offline)?;

        let operation_id = OperationId::new_random();

        let (issuance_requests, outputs) = self.build_issuance(represent_amount(amount)).await;
        let output_count = outputs.len() as u64;
        let output_bundle = self
            .client_ctx
            .make_client_outputs(ClientOutputBundle::new(outputs));

        // Subscribe before submitting so we cannot miss the finalisation event.
        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        let finalize_range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(
                &tx,
                operation_id,
                TransactionBuilder::new().with_outputs(output_bundle),
            )
            .await
            .map_err(|_| SendECashError::InsufficientBalance)?;

        // Caller's outputs come first — output indices 0..output_count.
        let caller_range =
            OutPointRange::new(finalize_range.txid(), IdxRange::from(0..output_count));

        self.output_executor
            .add_state_machine_dbtx(
                &tx,
                MintOutputStateMachine {
                    common: OutputSMCommon {
                        operation_id,
                        range: Some(caller_range),
                        issuance_requests,
                    },
                    state: OutputSMState::Pending,
                },
            )
            .await;

        dbtx.commit().await;

        await_output_finalisation(&self.client_ctx, operation_id, caller_range).await;

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
                SendPaymentEvent {
                    amount,
                    ecash: base32::encode_prefixed(PICOMINT_PREFIX, &ecash),
                },
            )
            .await;

        let sender = self.balance_update_sender.clone();
        dbtx.on_commit(move || sender.send_replace(()));

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

        let spendable_notes = ecash.notes();
        let inputs = build_inputs(&spendable_notes);
        let input_bundle = self
            .client_ctx
            .make_client_inputs(ClientInputBundle::new(inputs));

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        if tx.insert(&RECEIVE_OPERATION, &operation_id, &()).is_some() {
            return Ok(operation_id);
        }

        let finalize_range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(
                &tx,
                operation_id,
                TransactionBuilder::new().with_inputs(input_bundle),
            )
            .await
            .map_err(|_| ReceiveECashError::InsufficientFunds)?;

        let txid = finalize_range.txid();

        // Caller's inputs come first — input indices 0..input_count.
        self.input_executor
            .add_state_machine_dbtx(
                &tx,
                InputStateMachine {
                    common: input::InputSMCommon {
                        operation_id,
                        txid,
                        spendable_notes,
                    },
                    state: input::InputSMState::Pending,
                },
            )
            .await;

        self.receive_executor
            .add_state_machine_dbtx(
                &tx,
                ReceiveStateMachine {
                    common: crate::receive::ReceiveSMCommon { operation_id, txid },
                    state: crate::receive::ReceiveSMState::Pending,
                },
            )
            .await;

        self.client_ctx
            .log_event(
                &tx,
                operation_id,
                ReceivePaymentEvent {
                    amount: ecash.amount(),
                },
            )
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

        if let Ok(data) = module_api
            .fetch_recovery_slice(peer, TIMEOUT, start, end)
            .await
        {
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

async fn await_output_finalisation(
    client_ctx: &picomint_client_module::module::ClientContext<MintClientModule>,
    operation_id: OperationId,
    range: OutPointRange,
) {
    use futures::StreamExt as _;

    let mut stream =
        client_ctx.subscribe_operation_events_typed::<events::OutputFinalisedEvent>(operation_id);
    while let Some(ev) = stream.next().await {
        if ev.range == range {
            return;
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

fn represent_amount_with_fees(
    mut remaining_amount: Amount,
    output_fee: Amount,
) -> Vec<Denomination> {
    let mut denominations = Vec::new();

    // Add denominations with a greedy algorithm
    for denomination in client_denominations().rev() {
        let n_add = remaining_amount / (denomination.amount() + output_fee);

        denominations.extend(std::iter::repeat_n(denomination, n_add as usize));

        remaining_amount -= n_add * (denomination.amount() + output_fee);
    }

    // We sort the notes by amount to minimize the leaked information.
    denominations.sort();

    denominations
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
