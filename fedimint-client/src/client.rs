use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::future::Future;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context as _, bail};
use bitcoin::key::Secp256k1;
use bitcoin::secp256k1::{self, PublicKey};
use fedimint_api_client::api::{DynGlobalApi, IRawFederationApi};
use fedimint_api_client::transaction::Transaction;
use fedimint_api_client::{Endpoint, wire};
use fedimint_client_module::executor::ModuleExecutor;
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::module::{ClientContextIface, ClientModule, IdxRange, OutPointRange};
use fedimint_client_module::transaction::{
    TransactionBuilder, TxSubmissionStates, TxSubmissionStatesSM,
};
use fedimint_client_module::{
    ClientModuleInstance, ModuleRecoveryCompleted, TxAcceptedEvent, TxCreatedEvent, TxRejectedEvent,
};
use fedimint_core::config::{ClientConfig, FederationId, JsonClientConfig};
use fedimint_core::core::{ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::encoding::Encodable as _;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::task::TaskGroup;
use fedimint_core::util::{BoxStream, FmtCompactAnyhow as _, SafeUrl};
use fedimint_core::{
    Amount, PeerId, TransactionId, apply, async_trait_maybe_send, maybe_add_send,
    maybe_add_send_sync,
};
use fedimint_eventlog::{Event, EventKind, EventLogId, PersistedLogEntry};
use fedimint_gwv2_client::GatewayClientModuleV2;
use fedimint_lnv2_client::LightningClientModule;
use fedimint_logging::{LOG_CLIENT, LOG_CLIENT_NET_API, LOG_CLIENT_RECOVERY};
use fedimint_mintv2_client::MintClientModule;
use fedimint_redb::{Database, WriteTxRef};
use fedimint_walletv2_client::WalletClientModule;
use futures::{Stream, StreamExt as _};
use tokio::sync::watch;
use tokio_stream::wrappers::WatchStream;
use tracing::{debug, info, warn};

use crate::ClientBuilder;
use crate::db::{
    CLIENT_CONFIG, CLIENT_MODULE_RECOVERY, ClientModuleRecovery, ClientModuleRecoveryState,
};

pub(crate) mod builder;
pub(crate) mod handle;

/// Lightning-module flavor mounted on a client. Regular federation clients
/// use `Regular`, while the gateway daemon mounts `Gateway`. The two variants
/// share `Common = LightningModuleTypes` on the federation, so they are
/// mutually exclusive at instance id `1`.
pub enum LnFlavor {
    Regular(Arc<LightningClientModule>),
    Gateway(Arc<GatewayClientModuleV2>),
}

impl LnFlavor {
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn Any)) {
        match self {
            LnFlavor::Regular(m) => &**m,
            LnFlavor::Gateway(m) => &**m,
        }
    }

    fn input_fee(
        &self,
        amount: Amount,
        input: &fedimint_lnv2_common::LightningInput,
    ) -> Option<Amount> {
        match self {
            LnFlavor::Regular(m) => m.input_fee(amount, input),
            LnFlavor::Gateway(m) => m.input_fee(amount, input),
        }
    }

    fn output_fee(
        &self,
        amount: Amount,
        output: &fedimint_lnv2_common::LightningOutput,
    ) -> Option<Amount> {
        match self {
            LnFlavor::Regular(m) => m.output_fee(amount, output),
            LnFlavor::Gateway(m) => m.output_fee(amount, output),
        }
    }

    async fn start(&self) {
        match self {
            LnFlavor::Regular(m) => m.start().await,
            LnFlavor::Gateway(m) => m.start().await,
        }
    }
}

/// Main client type
///
/// A handle and API to interacting with a single federation. End user
/// applications that want to support interacting with multiple federations at
/// the same time, will need to instantiate and manage multiple instances of
/// this struct.
///
/// Under the hood it is starting and managing service tasks, state machines,
/// database and other resources required.
///
/// This type is shared externally and internally, and
/// [`crate::ClientHandle`] is responsible for external lifecycle management
/// and resource freeing of the [`Client`].
pub struct Client {
    config: tokio::sync::RwLock<ClientConfig>,
    decoders: ModuleDecoderRegistry,
    connectors: Endpoint,
    db: Database,
    federation_id: FederationId,
    federation_config_meta: BTreeMap<String, String>,
    pub(crate) mint: Arc<MintClientModule>,
    pub(crate) wallet: Arc<WalletClientModule>,
    pub(crate) ln: LnFlavor,
    tx_submission_executor: ModuleExecutor<TxSubmissionStatesSM>,
    pub(crate) api: DynGlobalApi,
    secp_ctx: Secp256k1<secp256k1::All>,

    task_group: TaskGroup,

    /// Updates about client recovery progress
    client_recovery_progress_receiver:
        watch::Receiver<BTreeMap<ModuleInstanceId, RecoveryProgress>>,

    /// Broadcast channel that pings on every committed event log insertion.
    /// `.subscribe()` hands out a fresh receiver to each waiter.
    log_event_added_tx: watch::Sender<()>,
}

impl Client {
    /// Initialize a client builder that can be configured to create a new
    /// client.
    pub async fn builder() -> anyhow::Result<ClientBuilder> {
        Ok(ClientBuilder::new())
    }

    pub fn api(&self) -> &(dyn IRawFederationApi + 'static) {
        &*self.api
    }

    pub fn api_clone(&self) -> DynGlobalApi {
        self.api.clone()
    }

    /// Returns a stream that emits the current connection status of all peers
    /// whenever any peer's status changes. Emits initial state immediately.
    pub fn connection_status_stream(&self) -> impl Stream<Item = BTreeMap<PeerId, bool>> {
        self.api.connection_status_stream()
    }

    /// Get the [`TaskGroup`] that is tied to Client's lifetime.
    pub fn task_group(&self) -> &TaskGroup {
        &self.task_group
    }

    pub async fn get_config_from_db(db: &Database) -> Option<ClientConfig> {
        db.begin_read().await.as_ref().get(&CLIENT_CONFIG, &())
    }

    pub async fn is_initialized(db: &Database) -> bool {
        Self::get_config_from_db(db).await.is_some()
    }

    pub fn federation_id(&self) -> FederationId {
        self.federation_id
    }

    pub async fn config(&self) -> ClientConfig {
        self.config.read().await.clone()
    }

    /// Returns the core API version that the federation supports
    ///
    /// This reads from the cached version stored during client initialization.
    /// If no cache is available (e.g., during initial setup), returns a default
    /// version (0, 0).
    pub fn decoders(&self) -> &ModuleDecoderRegistry {
        &self.decoders
    }

    /// Returns the module at the given instance id as `&dyn Any`, or panics
    /// if the instance id doesn't match the fixed module set.
    fn get_module_any(&self, instance: ModuleInstanceId) -> &(maybe_add_send_sync!(dyn Any)) {
        match instance {
            wire::MINT_INSTANCE_ID => &*self.mint,
            wire::LN_INSTANCE_ID => self.ln.as_any(),
            wire::WALLET_INSTANCE_ID => &*self.wallet,
            _ => panic!("Module instance {instance} not found"),
        }
    }

    /// Returns the input amount and output amount of a transaction
    ///
    /// # Panics
    /// If any of the input or output versions in the transaction builder are
    /// unknown by the respective module.
    fn transaction_builder_get_balance(&self, builder: &TransactionBuilder) -> (Amount, Amount) {
        // FIXME: prevent overflows, currently not suitable for untrusted input
        let mut in_amount = Amount::ZERO;
        let mut out_amount = Amount::ZERO;
        let mut fee_amount = Amount::ZERO;

        for input in builder.inputs() {
            let item_fee = match &input.input {
                wire::Input::Mint(i) => self.mint.input_fee(input.amount, i),
                wire::Input::Ln(i) => self.ln.input_fee(input.amount, i),
                wire::Input::Wallet(i) => self.wallet.input_fee(input.amount, i),
            }
            .expect(
                "We only build transactions with input versions that are supported by the module",
            );

            in_amount += input.amount;
            fee_amount += item_fee;
        }

        for output in builder.outputs() {
            let item_fee = match &output.output {
                wire::Output::Mint(o) => self.mint.output_fee(output.amount, o),
                wire::Output::Ln(o) => self.ln.output_fee(output.amount, o),
                wire::Output::Wallet(o) => self.wallet.output_fee(output.amount, o),
            }
            .expect(
                "We only build transactions with output versions that are supported by the module",
            );

            out_amount += output.amount;
            fee_amount += item_fee;
        }

        out_amount += fee_amount;
        (in_amount, out_amount)
    }

    pub fn get_internal_payment_markers(&self) -> anyhow::Result<(PublicKey, u64)> {
        Ok((self.federation_id().to_fake_ln_pub_key(&self.secp_ctx)?, 0))
    }

    /// Get metadata value from the federation config itself
    pub fn get_config_meta(&self, key: &str) -> Option<String> {
        self.federation_config_meta.get(key).cloned()
    }

    /// Adds funding to a transaction or removes over-funding via change.
    ///
    /// Returns the built transaction, the primary module's post-finalize
    /// spawn callback (if any) and its associated input/output ranges.
    async fn finalize_transaction(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        mut partial_transaction: TransactionBuilder,
    ) -> anyhow::Result<(
        Transaction,
        Option<fedimint_client_module::module::SpawnSms>,
        Range<u64>, // primary input range
        Range<u64>, // primary output range (today's change_range)
    )> {
        let (in_amount, out_amount) = self.transaction_builder_get_balance(&partial_transaction);

        let primary_input_start = partial_transaction.inputs().count() as u64;
        let primary_output_start = partial_transaction.outputs().count() as u64;

        let mut spawn_sms: Option<fedimint_client_module::module::SpawnSms> = None;
        let mut primary_input_end = primary_input_start;
        let mut primary_output_end = primary_output_start;

        if in_amount != out_amount {
            let contribution = self
                .mint
                .create_final_inputs_and_outputs(
                    &dbtx.isolate(format!("module-{}", wire::MINT_INSTANCE_ID)),
                    operation_id,
                    in_amount,
                    out_amount,
                )
                .await?;

            primary_input_end = primary_input_start + contribution.inputs.len() as u64;
            primary_output_end = primary_output_start + contribution.outputs.len() as u64;

            let wire_inputs = contribution
                .inputs
                .into_iter()
                .map(fedimint_client_module::transaction::ClientInput::into_wire)
                .collect();
            let wire_outputs = contribution
                .outputs
                .into_iter()
                .map(fedimint_client_module::transaction::ClientOutput::into_wire)
                .collect();

            partial_transaction = partial_transaction.with_inputs(
                fedimint_client_module::transaction::ClientInputBundle::new(wire_inputs),
            );
            partial_transaction = partial_transaction.with_outputs(
                fedimint_client_module::transaction::ClientOutputBundle::new(wire_outputs),
            );
            spawn_sms = Some(contribution.spawn_sms);
        }

        let (input_amount, output_amount) =
            self.transaction_builder_get_balance(&partial_transaction);

        assert!(input_amount >= output_amount, "Transaction is underfunded");

        let tx = partial_transaction.build(&self.secp_ctx);

        Ok((
            tx,
            spawn_sms,
            primary_input_start..primary_input_end,
            primary_output_start..primary_output_end,
        ))
    }

    /// Add funding and/or change to the transaction builder as needed, finalize
    /// the transaction and submit it to the federation.
    ///
    /// ## Errors
    /// The function will return an error if the operation with given ID already
    /// exists.
    ///
    /// ## Panics
    /// The function will panic if the database transaction collides with
    /// other and fails with others too often, this should not happen except for
    /// excessively concurrent scenarios.
    pub async fn finalize_and_submit_transaction(
        &self,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        let dbtx = self.db.begin_write().await;
        let result = self
            .finalize_and_submit_transaction_dbtx(&dbtx.as_ref(), operation_id, tx_builder)
            .await?;
        dbtx.commit().await;
        Ok(result)
    }

    /// See [`Self::finalize_and_submit_transaction`], just inside a database
    /// transaction.
    pub async fn finalize_and_submit_transaction_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        self.finalize_and_submit_transaction_inner(dbtx, operation_id, tx_builder)
            .await
    }

    async fn finalize_and_submit_transaction_inner(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        let (transaction, spawn_sms, primary_input_range, primary_output_range) = self
            .finalize_transaction(dbtx, operation_id, tx_builder)
            .await?;

        if transaction.consensus_encode_to_vec().len() > Transaction::MAX_TX_SIZE {
            let inputs = transaction
                .inputs
                .iter()
                .map(|i| i.module_instance_id())
                .collect::<Vec<_>>();
            let outputs = transaction
                .outputs
                .iter()
                .map(|o| o.module_instance_id())
                .collect::<Vec<_>>();
            warn!(
                target: LOG_CLIENT_NET_API,
                size=%transaction.consensus_encode_to_vec().len(),
                ?inputs,
                ?outputs,
                "Transaction too large",
            );
            debug!(target: LOG_CLIENT_NET_API, ?transaction, "transaction details");
            bail!(
                "The generated transaction would be rejected by the federation for being too large."
            );
        }

        let txid = transaction.tx_hash();

        debug!(
            target: LOG_CLIENT_NET_API,
            %txid,
            operation_id = %operation_id.fmt_short(),
            ?transaction,
            "Finalized and submitting transaction",
        );

        if let Some(spawn_sms) = spawn_sms {
            spawn_sms(
                dbtx,
                txid,
                IdxRange::from(primary_input_range),
                IdxRange::from(primary_output_range.clone()),
            )
            .await;
        }

        self.tx_submission_executor
            .add_state_machine_dbtx(
                dbtx,
                TxSubmissionStatesSM {
                    operation_id,
                    state: TxSubmissionStates::Created(transaction),
                },
            )
            .await;

        self.log_event_dbtx(dbtx, None, Some(operation_id), TxCreatedEvent { txid });

        Ok(OutPointRange::new(
            txid,
            IdxRange::from(primary_output_range),
        ))
    }

    pub async fn await_tx_accepted(
        &self,
        operation_id: OperationId,
        query_txid: TransactionId,
    ) -> Result<(), String> {
        let mut stream = self.subscribe_operation_events(operation_id);
        while let Some(entry) = stream.next().await {
            if let Some(ev) = entry.to_event::<TxAcceptedEvent>()
                && ev.txid == query_txid
            {
                return Ok(());
            }
            if let Some(ev) = entry.to_event::<TxRejectedEvent>()
                && ev.txid == query_txid
            {
                return Err(ev.error);
            }
        }
        unreachable!("subscribe_operation_events only ends at client shutdown")
    }

    /// Returns a typed module client instance by type. Uses `TypeId` dispatch
    /// over the fixed module set (`MintClientModule` / `WalletClientModule` /
    /// `LightningClientModule` / `GatewayClientModuleV2`).
    pub fn get_first_module<M: ClientModule>(
        &'_ self,
    ) -> anyhow::Result<ClientModuleInstance<'_, M>> {
        let tid = TypeId::of::<M>();
        let (module_any, id): (&(maybe_add_send_sync!(dyn Any)), ModuleInstanceId) =
            if tid == TypeId::of::<MintClientModule>() {
                (&*self.mint, wire::MINT_INSTANCE_ID)
            } else if tid == TypeId::of::<WalletClientModule>() {
                (&*self.wallet, wire::WALLET_INSTANCE_ID)
            } else if tid == TypeId::of::<LightningClientModule>() {
                match &self.ln {
                    LnFlavor::Regular(m) => (&**m, wire::LN_INSTANCE_ID),
                    LnFlavor::Gateway(_) => {
                        bail!("LightningClientModule is not mounted on this client")
                    }
                }
            } else if tid == TypeId::of::<GatewayClientModuleV2>() {
                match &self.ln {
                    LnFlavor::Gateway(m) => (&**m, wire::LN_INSTANCE_ID),
                    LnFlavor::Regular(_) => {
                        bail!("GatewayClientModuleV2 is not mounted on this client")
                    }
                }
            } else {
                bail!("No modules found of kind {}", M::kind());
            };

        let module: &M = module_any
            .downcast_ref::<M>()
            .expect("TypeId of M was just matched");
        let db = self.db().isolate(format!("module-{id}"));
        Ok(ClientModuleInstance {
            id,
            db,
            api: self.api().with_module(id),
            module,
        })
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn endpoints(&self) -> &Endpoint {
        &self.connectors
    }

    /// Returns the config of the client in JSON format.
    ///
    /// Compared to the consensus module format where module configs are binary
    /// encoded this format cannot be cryptographically verified but is easier
    /// to consume and to some degree human-readable.
    pub async fn get_config_json(&self) -> JsonClientConfig {
        self.config().await.to_json()
    }

    pub async fn get_balance(&self) -> anyhow::Result<Amount> {
        let dbtx = self.db().begin_write().await;
        Ok(self
            .mint
            .get_balance(
                &dbtx
                    .as_ref()
                    .isolate(format!("module-{}", wire::MINT_INSTANCE_ID)),
            )
            .await)
    }

    /// Returns a stream that yields the current client balance every time it
    /// changes.
    pub async fn subscribe_balance_changes(&self) -> BoxStream<'static, Amount> {
        let mut balance_changes = self.mint.subscribe_balance_changes().await;
        let initial_balance = self.get_balance().await.expect("Primary is present");
        let mint = self.mint.clone();
        let db = self.db().clone();

        Box::pin(async_stream::stream! {
            yield initial_balance;
            let mut prev_balance = initial_balance;
            while let Some(()) = balance_changes.next().await {
                let dbtx = db.begin_write().await;
                let balance = mint
                    .get_balance(
                        &dbtx.as_ref().isolate(format!("module-{}", wire::MINT_INSTANCE_ID)),
                    )
                    .await;

                // Deduplicate in case modules cannot always tell if the balance actually changed
                if balance != prev_balance {
                    prev_balance = balance;
                    yield balance;
                }
            }
        })
    }

    pub fn has_pending_recoveries(&self) -> bool {
        !self
            .client_recovery_progress_receiver
            .borrow()
            .iter()
            .all(|(_id, progress)| progress.is_done())
    }

    /// Wait for all module recoveries to finish
    ///
    /// This will block until the recovery task is done with recoveries.
    /// Returns success if all recovery tasks are complete (success case),
    /// or an error if some modules could not complete the recovery at the time.
    ///
    /// A bit of a heavy approach.
    pub async fn wait_for_all_recoveries(&self) -> anyhow::Result<()> {
        let mut recovery_receiver = self.client_recovery_progress_receiver.clone();
        recovery_receiver
            .wait_for(|in_progress| {
                in_progress
                    .iter()
                    .all(|(_id, progress)| progress.is_done())
            })
            .await
            .context("Recovery task completed and update receiver disconnected, but some modules failed to recover")?;

        Ok(())
    }

    /// Subscribe to recover progress for all the modules.
    ///
    /// This stream can contain duplicate progress for a module.
    /// Don't use this stream for detecting completion of recovery.
    pub fn subscribe_to_recovery_progress(
        &self,
    ) -> impl Stream<Item = (ModuleInstanceId, RecoveryProgress)> + use<> {
        WatchStream::new(self.client_recovery_progress_receiver.clone())
            .flat_map(futures::stream::iter)
    }

    pub async fn wait_for_module_kind_recovery(
        &self,
        module_kind: ModuleKind,
    ) -> anyhow::Result<()> {
        let mut recovery_receiver = self.client_recovery_progress_receiver.clone();
        let config = self.config().await;
        recovery_receiver
            .wait_for(|in_progress| {
                !in_progress
                    .iter()
                    .filter(|(module_instance_id, _progress)| {
                        config.modules[module_instance_id].kind == module_kind
                    })
                    .any(|(_id, progress)| !progress.is_done())
            })
            .await
            .context("Recovery task completed and update receiver disconnected, but the desired modules are still unavailable or failed to recover")?;

        Ok(())
    }

    fn spawn_module_recoveries_task(
        &self,
        recovery_sender: watch::Sender<BTreeMap<ModuleInstanceId, RecoveryProgress>>,
        module_recoveries: BTreeMap<
            ModuleInstanceId,
            Pin<Box<maybe_add_send!(dyn Future<Output = anyhow::Result<()>>)>>,
        >,
        module_recovery_progress_receivers: BTreeMap<
            ModuleInstanceId,
            watch::Receiver<RecoveryProgress>,
        >,
    ) {
        let db = self.db.clone();
        let log_event_added_tx = self.log_event_added_tx.clone();
        let module_kinds: BTreeMap<ModuleInstanceId, String> = [
            (wire::MINT_INSTANCE_ID, "mintv2".to_string()),
            (wire::LN_INSTANCE_ID, "lnv2".to_string()),
            (wire::WALLET_INSTANCE_ID, "walletv2".to_string()),
        ]
        .into();
        let task_group = self.task_group.clone();
        self.task_group
            .spawn("module recoveries", |_task_handle| async move {
                Self::run_module_recoveries_task(
                    db,
                    log_event_added_tx,
                    recovery_sender,
                    module_recoveries,
                    module_recovery_progress_receivers,
                    module_kinds,
                    task_group,
                )
                .await;
            });
    }

    async fn run_module_recoveries_task(
        db: Database,
        log_event_added_tx: watch::Sender<()>,
        recovery_sender: watch::Sender<BTreeMap<ModuleInstanceId, RecoveryProgress>>,
        module_recoveries: BTreeMap<
            ModuleInstanceId,
            Pin<Box<maybe_add_send!(dyn Future<Output = anyhow::Result<()>>)>>,
        >,
        module_recovery_progress_receivers: BTreeMap<
            ModuleInstanceId,
            watch::Receiver<RecoveryProgress>,
        >,
        module_kinds: BTreeMap<ModuleInstanceId, String>,
        task_group: TaskGroup,
    ) {
        debug!(target: LOG_CLIENT_RECOVERY, num_modules=%module_recovery_progress_receivers.len(), "Starting module recoveries");

        // Recoveries run as independent tasks and report completion via this
        // channel. Running them inside the select below would let them hold a
        // write transaction while yielding, deadlocking the progress handler
        // that needs its own write transaction.
        let (completion_tx, completion_rx) =
            tokio::sync::mpsc::unbounded_channel::<ModuleInstanceId>();

        for (module_instance_id, f) in module_recoveries {
            let tx = completion_tx.clone();
            task_group.spawn(
                format!("module {module_instance_id} recovery"),
                move |_| async move {
                    match f.await {
                        Ok(()) => {
                            let _ = tx.send(module_instance_id);
                        }
                        Err(err) => {
                            warn!(
                                target: LOG_CLIENT,
                                err = %err.fmt_compact_anyhow(),
                                module_instance_id,
                                "Module recovery failed"
                            );
                            // Don't send completion - recovery is considered
                            // permanently stuck.
                        }
                    }
                },
            );
        }
        drop(completion_tx);

        let progress_stream = futures::stream::FuturesUnordered::new();
        for (module_instance_id, rx) in module_recovery_progress_receivers {
            progress_stream.push(
                tokio_stream::wrappers::WatchStream::new(rx)
                    .fuse()
                    .map(move |progress| (module_instance_id, Some(progress))),
            );
        }

        let completion_stream = tokio_stream::wrappers::UnboundedReceiverStream::new(completion_rx)
            .map(|module_instance_id| (module_instance_id, None));

        let mut futures = futures::stream::select(
            futures::stream::select_all(progress_stream),
            completion_stream,
        );

        while let Some((module_instance_id, progress)) = futures.next().await {
            let dbtx = db.begin_write().await;
            let tx = dbtx.as_ref();

            let prev_progress = *recovery_sender
                .borrow()
                .get(&module_instance_id)
                .expect("existing progress must be present");

            let progress = if prev_progress.is_done() {
                // since updates might be out of order, once done, stick with it
                prev_progress
            } else if let Some(progress) = progress {
                progress
            } else {
                prev_progress.to_complete()
            };

            if !prev_progress.is_done() && progress.is_done() {
                info!(
                    target: LOG_CLIENT,
                    module_instance_id,
                    progress = format!("{}/{}", progress.complete, progress.total),
                    "Recovery complete"
                );
                fedimint_eventlog::log_event(
                    &tx,
                    log_event_added_tx.clone(),
                    None,
                    None,
                    ModuleRecoveryCompleted {
                        module_id: module_instance_id,
                    },
                );
            } else {
                info!(
                    target: LOG_CLIENT,
                    module_instance_id,
                    kind = module_kinds.get(&module_instance_id).map(String::as_str).unwrap_or("unknown"),
                    progress = format!("{}/{}", progress.complete, progress.total),
                    "Recovery progress"
                );
            }

            tx.insert(
                &CLIENT_MODULE_RECOVERY,
                &ClientModuleRecovery { module_instance_id },
                &ClientModuleRecoveryState { progress },
            );
            dbtx.commit().await;

            recovery_sender.send_modify(|v| {
                v.insert(module_instance_id, progress);
            });
        }
        debug!(target: LOG_CLIENT_RECOVERY, "Recovery executor stopped");
    }

    /// Returns a list of guardian API URLs
    pub async fn get_peer_urls(&self) -> BTreeMap<PeerId, SafeUrl> {
        self.config()
            .await
            .global
            .api_endpoints
            .iter()
            .map(|(peer, endpoint)| (*peer, endpoint.url.clone()))
            .collect()
    }

    /// Create an invite code with the api endpoint of the given peer which can
    /// be used to download this client config
    pub async fn invite_code(&self, peer: PeerId) -> Option<InviteCode> {
        self.get_peer_urls()
            .await
            .into_iter()
            .find_map(|(peer_id, url)| (peer == peer_id).then_some(url))
            .map(|peer_url| InviteCode::new(peer_url.clone(), peer, self.federation_id()))
    }

    /// Returns the guardian public key set from the client config.
    pub async fn get_guardian_public_keys_blocking(
        &self,
    ) -> BTreeMap<PeerId, fedimint_core::secp256k1::PublicKey> {
        self.config()
            .await
            .global
            .broadcast_public_keys
            .expect("Guardian public keys must be present in config")
    }

    pub fn log_event_dbtx<E>(
        &self,
        dbtx: &WriteTxRef<'_>,
        module_id: Option<ModuleInstanceId>,
        operation_id: Option<OperationId>,
        event: E,
    ) where
        E: Event + Send,
    {
        fedimint_eventlog::log_event(
            dbtx,
            self.log_event_added_tx.clone(),
            module_id,
            operation_id,
            event,
        );
    }

    pub fn log_event_raw_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        kind: EventKind,
        module: Option<(ModuleKind, ModuleInstanceId)>,
        operation_id: Option<OperationId>,
        payload: Vec<u8>,
    ) {
        let module_id = module.as_ref().map(|m| m.1);
        let module_kind = module.map(|m| m.0);
        fedimint_eventlog::log_event_raw(
            dbtx,
            self.log_event_added_tx.clone(),
            kind,
            module_kind,
            module_id,
            operation_id,
            payload,
        );
    }

    pub async fn get_event_log(
        &self,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        let pos = pos.unwrap_or(EventLogId::LOG_START);
        let end = pos.saturating_add(limit);
        self.db
            .begin_read()
            .await
            .as_ref()
            .with_native_table(&fedimint_eventlog::EVENT_LOG, |t| {
                t.range(pos..end)
                    .expect("redb range failed")
                    .map(|r| {
                        let (k, v) = r.expect("redb range item failed");
                        fedimint_eventlog::PersistedLogEntry::new(k.value(), v.value())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// Get a receiver that signals when new events are added to the event log
    pub fn log_event_added_rx(&self) -> watch::Receiver<()> {
        self.log_event_added_tx.subscribe()
    }

    /// Stream every event belonging to `operation_id`, starting from the
    /// beginning of the log (existing events first, then live ones).
    pub fn subscribe_operation_events(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, PersistedLogEntry> {
        Box::pin(fedimint_eventlog::subscribe_operation_events(
            self.db.clone(),
            self.log_event_added_tx.subscribe(),
            operation_id,
        ))
    }
}

#[apply(async_trait_maybe_send!)]
impl ClientContextIface for Client {
    fn get_module(&self, instance: ModuleInstanceId) -> &(maybe_add_send_sync!(dyn Any)) {
        Client::get_module_any(self, instance)
    }

    fn api_clone(&self) -> DynGlobalApi {
        Client::api_clone(self)
    }
    fn decoders(&self) -> &ModuleDecoderRegistry {
        Client::decoders(self)
    }

    async fn finalize_and_submit_transaction(
        &self,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        Client::finalize_and_submit_transaction(self, operation_id, tx_builder).await
    }

    async fn finalize_and_submit_transaction_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        Client::finalize_and_submit_transaction_dbtx(self, dbtx, operation_id, tx_builder).await
    }

    async fn finalize_and_submit_transaction_inner(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        Client::finalize_and_submit_transaction_inner(self, dbtx, operation_id, tx_builder).await
    }

    async fn config(&self) -> ClientConfig {
        Client::config(self).await
    }

    fn db(&self) -> &Database {
        Client::db(self)
    }

    async fn invite_code(&self, peer: PeerId) -> Option<InviteCode> {
        Client::invite_code(self, peer).await
    }

    fn get_internal_payment_markers(&self) -> anyhow::Result<(PublicKey, u64)> {
        Client::get_internal_payment_markers(self)
    }

    fn log_event_added_tx(&self) -> watch::Sender<()> {
        self.log_event_added_tx.clone()
    }

    async fn get_event_log(&self, pos: Option<EventLogId>, limit: u64) -> Vec<PersistedLogEntry> {
        Client::get_event_log(self, pos, limit).await
    }

    fn subscribe_operation_events(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, PersistedLogEntry> {
        Client::subscribe_operation_events(self, operation_id)
    }

    async fn await_tx_accepted(
        &self,
        operation_id: OperationId,
        txid: TransactionId,
    ) -> Result<(), String> {
        Client::await_tx_accepted(self, operation_id, txid).await
    }
}

// TODO: impl `Debug` for `Client` and derive here
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Client")
    }
}
