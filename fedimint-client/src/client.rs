use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::future::{Future, pending};
use std::ops::Range;
use std::pin::Pin;

use anyhow::{Context as _, anyhow, bail, format_err};
use bitcoin::key::Secp256k1;
use bitcoin::key::rand::thread_rng;
use bitcoin::secp256k1::{self, PublicKey};
use fedimint_api_client::api::{DynGlobalApi, IGlobalFederationApi};
use fedimint_api_client::Endpoint;
use fedimint_client_module::executor::ModuleExecutor;
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::module::{
    ClientContextIface, ClientModule, ClientModuleRegistry, DynClientModule, IClientModule,
    IdxRange, OutPointRange,
};
use fedimint_client_module::secret::{PlainRootSecretStrategy, RootSecretStrategy as _};
use fedimint_client_module::transaction::{
    TransactionBuilder, TxSubmissionStates, TxSubmissionStatesSM,
};
use fedimint_client_module::{
    ClientModuleInstance, ModuleRecoveryCompleted, TxAcceptedEvent, TxCreatedEvent, TxRejectedEvent,
};
use fedimint_core::config::{ClientConfig, FederationId, JsonClientConfig, ModuleInitRegistry};
use fedimint_core::core::{DynInput, DynOutput, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::{
    AutocommitError, Database, DatabaseRecord, DatabaseTransaction,
    IDatabaseTransactionOpsCore as _, IDatabaseTransactionOpsCoreTyped as _, NonCommittable,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::envs::is_running_in_test_env;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::task::TaskGroup;
use fedimint_core::transaction::Transaction;
use fedimint_core::util::{BoxStream, FmtCompactAnyhow as _, SafeUrl};
use fedimint_core::{
    Amount, PeerId, TransactionId, apply, async_trait_maybe_send, maybe_add_send,
    maybe_add_send_sync,
};
use fedimint_eventlog::{
    DBTransactionEventLogExt as _, DynEventLogTrimableTracker, Event, EventKind, EventLogEntry,
    EventLogId, EventLogTrimableId, EventLogTrimableTracker, EventPersistence, PersistedLogEntry,
};
use fedimint_logging::{LOG_CLIENT, LOG_CLIENT_NET_API, LOG_CLIENT_RECOVERY};
use futures::{Stream, StreamExt as _};
use tokio::sync::{broadcast, watch};
use tokio_stream::wrappers::WatchStream;
use tracing::{debug, info, warn};

use crate::ClientBuilder;
use crate::client::event_log::DefaultApplicationEventLogKey;
use crate::db::{
    ApiSecretKey, ClientConfigKey, ClientModuleRecovery, ClientModuleRecoveryState,
    EncodedClientSecretKey, apply_migrations_core_client_dbtx, get_decoded_client_secret,
    verify_client_db_integrity_dbtx,
};
use crate::module_init::{DynClientModuleInit, IClientModuleInit};

pub(crate) mod builder;
pub(crate) mod event_log;
pub(crate) mod handle;

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
    api_secret: Option<String>,
    decoders: ModuleDecoderRegistry,
    connectors: Endpoint,
    db: Database,
    federation_id: FederationId,
    federation_config_meta: BTreeMap<String, String>,
    primary_module: Option<ModuleInstanceId>,
    pub(crate) modules: ClientModuleRegistry,
    tx_submission_executor: ModuleExecutor<TxSubmissionStatesSM>,
    pub(crate) api: DynGlobalApi,
    secp_ctx: Secp256k1<secp256k1::All>,

    task_group: TaskGroup,

    /// Updates about client recovery progress
    client_recovery_progress_receiver:
        watch::Receiver<BTreeMap<ModuleInstanceId, RecoveryProgress>>,

    /// Internal client sender to wake up log ordering task every time a
    /// (unuordered) log event is added.
    log_ordering_wakeup_tx: watch::Sender<()>,
    /// Receiver for events fired every time (ordered) log event is added.
    log_event_added_rx: watch::Receiver<()>,
    log_event_added_transient_tx: broadcast::Sender<EventLogEntry>,
}

impl Client {
    /// Initialize a client builder that can be configured to create a new
    /// client.
    pub async fn builder() -> anyhow::Result<ClientBuilder> {
        Ok(ClientBuilder::new())
    }

    pub fn api(&self) -> &(dyn IGlobalFederationApi + 'static) {
        self.api.as_ref()
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
        let mut dbtx = db.begin_transaction_nc().await;
        dbtx.get_value(&ClientConfigKey).await
    }

    pub async fn get_api_secret_from_db(db: &Database) -> Option<String> {
        let mut dbtx = db.begin_transaction_nc().await;
        dbtx.get_value(&ApiSecretKey).await
    }

    pub async fn store_encodable_client_secret<T: Encodable>(
        db: &Database,
        secret: T,
    ) -> anyhow::Result<()> {
        let mut dbtx = db.begin_transaction().await;

        // Don't overwrite an existing secret
        if dbtx.get_value(&EncodedClientSecretKey).await.is_some() {
            bail!("Encoded client secret already exists, cannot overwrite")
        }

        let encoded_secret = T::consensus_encode_to_vec(&secret);
        dbtx.insert_entry(&EncodedClientSecretKey, &encoded_secret)
            .await;
        dbtx.commit_tx().await;
        Ok(())
    }

    pub async fn load_decodable_client_secret<T: Decodable>(db: &Database) -> anyhow::Result<T> {
        let Some(secret) = Self::load_decodable_client_secret_opt(db).await? else {
            bail!("Encoded client secret not present in DB")
        };

        Ok(secret)
    }
    pub async fn load_decodable_client_secret_opt<T: Decodable>(
        db: &Database,
    ) -> anyhow::Result<Option<T>> {
        let mut dbtx = db.begin_transaction_nc().await;

        let client_secret = dbtx.get_value(&EncodedClientSecretKey).await;

        Ok(match client_secret {
            Some(client_secret) => Some(
                T::consensus_decode_whole(&client_secret, &ModuleRegistry::default())
                    .map_err(|e| anyhow!("Decoding failed: {e}"))?,
            ),
            None => None,
        })
    }

    pub async fn load_or_generate_client_secret(db: &Database) -> anyhow::Result<[u8; 64]> {
        let client_secret = match Self::load_decodable_client_secret::<[u8; 64]>(db).await {
            Ok(secret) => secret,
            _ => {
                let secret = PlainRootSecretStrategy::random(&mut thread_rng());
                Self::store_encodable_client_secret(db, secret)
                    .await
                    .expect("Storing client secret must work");
                secret
            }
        };
        Ok(client_secret)
    }

    pub async fn is_initialized(db: &Database) -> bool {
        let mut dbtx = db.begin_transaction_nc().await;
        dbtx.raw_get_bytes(&[ClientConfigKey::DB_PREFIX])
            .await
            .expect("Unrecoverable error occurred while reading and entry from the database")
            .is_some()
    }

    pub fn federation_id(&self) -> FederationId {
        self.federation_id
    }

    pub async fn config(&self) -> ClientConfig {
        self.config.read().await.clone()
    }

    // TODO: change to `-> Option<&str>`
    pub fn api_secret(&self) -> &Option<String> {
        &self.api_secret
    }

    /// Returns the core API version that the federation supports
    ///
    /// This reads from the cached version stored during client initialization.
    /// If no cache is available (e.g., during initial setup), returns a default
    /// version (0, 0).
    pub fn decoders(&self) -> &ModuleDecoderRegistry {
        &self.decoders
    }

    /// Returns a reference to the module, panics if not found
    fn get_module(&self, instance: ModuleInstanceId) -> &maybe_add_send_sync!(dyn IClientModule) {
        self.try_get_module(instance)
            .expect("Module instance not found")
    }

    fn try_get_module(
        &self,
        instance: ModuleInstanceId,
    ) -> Option<&maybe_add_send_sync!(dyn IClientModule)> {
        Some(self.modules.get(instance)?.as_ref())
    }

    pub fn has_module(&self, instance: ModuleInstanceId) -> bool {
        self.modules.get(instance).is_some()
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
            let module = self.get_module(input.input.module_instance_id());

            let item_fee = module.input_fee(input.amount, &input.input).expect(
                "We only build transactions with input versions that are supported by the module",
            );

            in_amount += input.amount;
            fee_amount += item_fee;
        }

        for output in builder.outputs() {
            let module = self.get_module(output.output.module_instance_id());

            let item_fee = module.output_fee(output.amount, &output.output).expect(
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
        dbtx: &mut DatabaseTransaction<'_>,
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
            let (module_id, module) = self
                .primary_module()
                .ok_or_else(|| anyhow!("No primary module to balance a partial transaction"))?;

            let contribution = module
                .create_final_inputs_and_outputs(
                    module_id,
                    dbtx,
                    operation_id,
                    in_amount,
                    out_amount,
                )
                .await?;

            primary_input_end = primary_input_start + contribution.inputs.len() as u64;
            primary_output_end = primary_output_start + contribution.outputs.len() as u64;

            partial_transaction = partial_transaction.with_inputs(
                fedimint_client_module::transaction::ClientInputBundle::new(contribution.inputs),
            );
            partial_transaction = partial_transaction.with_outputs(
                fedimint_client_module::transaction::ClientOutputBundle::new(contribution.outputs),
            );
            spawn_sms = Some(contribution.spawn_sms);
        }

        let (input_amount, output_amount) =
            self.transaction_builder_get_balance(&partial_transaction);

        assert!(input_amount >= output_amount, "Transaction is underfunded");

        let tx = partial_transaction.build(&self.secp_ctx, thread_rng());

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
        let autocommit_res = self
            .db
            .autocommit(
                |dbtx, _| {
                    let tx_builder = tx_builder.clone();
                    Box::pin(async move {
                        self.finalize_and_submit_transaction_dbtx(dbtx, operation_id, tx_builder)
                            .await
                    })
                },
                Some(100), // TODO: handle what happens after 100 retries
            )
            .await;

        match autocommit_res {
            Ok(txid) => Ok(txid),
            Err(AutocommitError::ClosureError { error, .. }) => Err(error),
            Err(AutocommitError::CommitFailed {
                attempts,
                last_error,
            }) => panic!(
                "Failed to commit tx submission dbtx after {attempts} attempts: {last_error}"
            ),
        }
    }

    /// See [`Self::finalize_and_submit_transaction`], just inside a database
    /// transaction.
    pub async fn finalize_and_submit_transaction_dbtx(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        self.finalize_and_submit_transaction_inner(dbtx, operation_id, tx_builder)
            .await
    }

    async fn finalize_and_submit_transaction_inner(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        let (transaction, spawn_sms, primary_input_range, primary_output_range) = self
            .finalize_transaction(&mut dbtx.to_ref_nc(), operation_id, tx_builder)
            .await?;

        if transaction.consensus_encode_to_vec().len() > Transaction::MAX_TX_SIZE {
            let inputs = transaction
                .inputs
                .iter()
                .map(DynInput::module_instance_id)
                .collect::<Vec<_>>();
            let outputs = transaction
                .outputs
                .iter()
                .map(DynOutput::module_instance_id)
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

        self.log_event_dbtx(dbtx, None, TxCreatedEvent { txid, operation_id })
            .await;

        Ok(OutPointRange::new(
            txid,
            IdxRange::from(primary_output_range),
        ))
    }

    pub async fn await_tx_accepted(&self, query_txid: TransactionId) -> Result<(), String> {
        let mut log_rx = self.log_event_added_rx();
        let mut next_id = EventLogId::LOG_START;
        loop {
            for entry in self.get_event_log(Some(next_id), 100).await {
                next_id = entry.id().saturating_add(1);
                let raw = entry.as_raw();
                if raw.module_kind().is_some() {
                    continue;
                }
                if raw.kind == TxAcceptedEvent::KIND
                    && let Some(ev) = raw.to_event::<TxAcceptedEvent>()
                    && ev.txid == query_txid
                {
                    return Ok(());
                }
                if raw.kind == TxRejectedEvent::KIND
                    && let Some(ev) = raw.to_event::<TxRejectedEvent>()
                    && ev.txid == query_txid
                {
                    return Err(ev.error);
                }
            }
            let _ = log_rx.changed().await;
        }
    }

    /// Returns a reference to a typed module client instance by kind
    pub fn get_first_module<M: ClientModule>(
        &'_ self,
    ) -> anyhow::Result<ClientModuleInstance<'_, M>> {
        let module_kind = M::kind();
        let id = self
            .get_first_instance(&module_kind)
            .ok_or_else(|| format_err!("No modules found of kind {module_kind}"))?;
        let module: &M = self
            .try_get_module(id)
            .ok_or_else(|| format_err!("Unknown module instance {id}"))?
            .as_any()
            .downcast_ref::<M>()
            .ok_or_else(|| format_err!("Module is not of type {}", std::any::type_name::<M>()))?;
        let db = self.db().with_prefix_module_id(id);
        Ok(ClientModuleInstance {
            id,
            db,
            api: self.api().with_module(id),
            module,
        })
    }

    pub fn get_module_client_dyn(
        &self,
        instance_id: ModuleInstanceId,
    ) -> anyhow::Result<&maybe_add_send_sync!(dyn IClientModule)> {
        self.try_get_module(instance_id)
            .ok_or(anyhow!("Unknown module instance {}", instance_id))
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn endpoints(&self) -> &Endpoint {
        &self.connectors
    }

    /// Returns the instance id of the first module of the given kind.
    pub fn get_first_instance(&self, module_kind: &ModuleKind) -> Option<ModuleInstanceId> {
        self.modules
            .iter_modules()
            .find(|(_, kind, _module)| *kind == module_kind)
            .map(|(instance_id, _, _)| instance_id)
    }

    /// Returns the data from which the client's root secret is derived (e.g.
    /// BIP39 seed phrase struct).
    pub async fn root_secret_encoding<T: Decodable>(&self) -> anyhow::Result<T> {
        get_decoded_client_secret::<T>(self.db()).await
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
        let (id, module) = self
            .primary_module()
            .ok_or_else(|| anyhow!("Primary module not available"))?;
        Ok(module
            .get_balance(id, &mut self.db().begin_transaction_nc().await)
            .await)
    }

    /// Returns a stream that yields the current client balance every time it
    /// changes.
    pub async fn subscribe_balance_changes(&self) -> BoxStream<'static, Amount> {
        let primary_module_things =
            if let Some((primary_module_id, primary_module)) = self.primary_module() {
                let balance_changes = primary_module.subscribe_balance_changes().await;
                let initial_balance = self.get_balance().await.expect("Primary is present");

                Some((
                    primary_module_id,
                    primary_module.clone(),
                    balance_changes,
                    initial_balance,
                ))
            } else {
                None
            };
        let db = self.db().clone();

        Box::pin(async_stream::stream! {
            let Some((primary_module_id, primary_module, mut balance_changes, initial_balance)) = primary_module_things else {
                // If there is no primary module, there will not be one until client is
                // restarted
                pending().await
            };


            yield initial_balance;
            let mut prev_balance = initial_balance;
            while let Some(()) = balance_changes.next().await {
                let mut dbtx = db.begin_transaction_nc().await;
                let balance = primary_module
                     .get_balance(primary_module_id, &mut dbtx)
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
        let log_ordering_wakeup_tx = self.log_ordering_wakeup_tx.clone();
        let module_kinds: BTreeMap<ModuleInstanceId, String> = self
            .modules
            .iter_modules_id_kind()
            .map(|(id, kind)| (id, kind.to_string()))
            .collect();
        self.task_group
            .spawn("module recoveries", |_task_handle| async {
                Self::run_module_recoveries_task(
                    db,
                    log_ordering_wakeup_tx,
                    recovery_sender,
                    module_recoveries,
                    module_recovery_progress_receivers,
                    module_kinds,
                )
                .await;
            });
    }

    async fn run_module_recoveries_task(
        db: Database,
        log_ordering_wakeup_tx: watch::Sender<()>,
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
    ) {
        debug!(target: LOG_CLIENT_RECOVERY, num_modules=%module_recovery_progress_receivers.len(), "Staring module recoveries");
        let mut completed_stream = Vec::new();
        let progress_stream = futures::stream::FuturesUnordered::new();

        for (module_instance_id, f) in module_recoveries {
            completed_stream.push(futures::stream::once(Box::pin(async move {
                match f.await {
                    Ok(()) => (module_instance_id, None),
                    Err(err) => {
                        warn!(
                            target: LOG_CLIENT,
                            err = %err.fmt_compact_anyhow(), module_instance_id, "Module recovery failed"
                        );
                        // a module recovery that failed reports and error and
                        // just never finishes, so we don't need a separate state
                        // for it
                        futures::future::pending::<()>().await;
                        unreachable!()
                    }
                }
            })));
        }

        for (module_instance_id, rx) in module_recovery_progress_receivers {
            progress_stream.push(
                tokio_stream::wrappers::WatchStream::new(rx)
                    .fuse()
                    .map(move |progress| (module_instance_id, Some(progress))),
            );
        }

        let mut futures = futures::stream::select(
            futures::stream::select_all(progress_stream),
            futures::stream::select_all(completed_stream),
        );

        while let Some((module_instance_id, progress)) = futures.next().await {
            let mut dbtx = db.begin_transaction().await;

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
                dbtx.log_event(
                    log_ordering_wakeup_tx.clone(),
                    None,
                    ModuleRecoveryCompleted {
                        module_id: module_instance_id,
                    },
                )
                .await;
            } else {
                info!(
                    target: LOG_CLIENT,
                    module_instance_id,
                    kind = module_kinds.get(&module_instance_id).map(String::as_str).unwrap_or("unknown"),
                    progress = format!("{}/{}", progress.complete, progress.total),
                    "Recovery progress"
                );
            }

            dbtx.insert_entry(
                &ClientModuleRecovery { module_instance_id },
                &ClientModuleRecoveryState { progress },
            )
            .await;
            dbtx.commit_tx().await;

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
            .map(|peer_url| {
                InviteCode::new(
                    peer_url.clone(),
                    peer,
                    self.federation_id(),
                    self.api_secret.clone(),
                )
            })
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

    pub async fn log_event<E>(&self, module_id: Option<ModuleInstanceId>, event: E)
    where
        E: Event + Send,
    {
        let mut dbtx = self.db.begin_transaction().await;
        self.log_event_dbtx(&mut dbtx, module_id, event).await;
        dbtx.commit_tx().await;
    }

    pub async fn log_event_dbtx<E, Cap>(
        &self,
        dbtx: &mut DatabaseTransaction<'_, Cap>,
        module_id: Option<ModuleInstanceId>,
        event: E,
    ) where
        E: Event + Send,
        Cap: Send,
    {
        dbtx.log_event(self.log_ordering_wakeup_tx.clone(), module_id, event)
            .await;
    }

    pub async fn log_event_raw_dbtx<Cap>(
        &self,
        dbtx: &mut DatabaseTransaction<'_, Cap>,
        kind: EventKind,
        module: Option<(ModuleKind, ModuleInstanceId)>,
        payload: Vec<u8>,
        persist: EventPersistence,
    ) where
        Cap: Send,
    {
        let module_id = module.as_ref().map(|m| m.1);
        let module_kind = module.map(|m| m.0);
        dbtx.log_event_raw(
            self.log_ordering_wakeup_tx.clone(),
            kind,
            module_kind,
            module_id,
            payload,
            persist,
        )
        .await;
    }

    /// Built in event log (trimmable) tracker
    ///
    /// For the convenience of downstream applications, [`Client`] can store
    /// internally event log position for the main application using/driving it.
    ///
    /// Note that this position is a singleton, so this tracker should not be
    /// used for multiple purposes or applications, etc. at the same time.
    ///
    /// If the application has a need to follow log using multiple trackers, it
    /// should implement own [`DynEventLogTrimableTracker`] and store its
    /// persient data by itself.
    pub fn built_in_application_event_log_tracker(&self) -> DynEventLogTrimableTracker {
        struct BuiltInApplicationEventLogTracker;

        #[apply(async_trait_maybe_send!)]
        impl EventLogTrimableTracker for BuiltInApplicationEventLogTracker {
            // Store position in the event log
            async fn store(
                &mut self,
                dbtx: &mut DatabaseTransaction<NonCommittable>,
                pos: EventLogTrimableId,
            ) -> anyhow::Result<()> {
                dbtx.insert_entry(&DefaultApplicationEventLogKey, &pos)
                    .await;
                Ok(())
            }

            /// Load the last previous stored position (or None if never stored)
            async fn load(
                &mut self,
                dbtx: &mut DatabaseTransaction<NonCommittable>,
            ) -> anyhow::Result<Option<EventLogTrimableId>> {
                Ok(dbtx.get_value(&DefaultApplicationEventLogKey).await)
            }
        }
        Box::new(BuiltInApplicationEventLogTracker)
    }

    /// Like [`Self::handle_events`] but for historical data.
    ///
    ///
    /// This function can be used to process subset of events
    /// that is infrequent and important enough to be persisted
    /// forever. Most applications should prefer to use [`Self::handle_events`]
    /// which emits *all* events.
    pub async fn handle_historical_events<F, R>(
        &self,
        tracker: fedimint_eventlog::DynEventLogTracker,
        handler_fn: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&mut DatabaseTransaction<NonCommittable>, EventLogEntry) -> R,
        R: Future<Output = anyhow::Result<()>>,
    {
        fedimint_eventlog::handle_events(
            self.db.clone(),
            tracker,
            self.log_event_added_rx.clone(),
            handler_fn,
        )
        .await
    }

    /// Handle events emitted by the client
    ///
    /// This is a preferred method for reactive & asynchronous
    /// processing of events emitted by the client.
    ///
    /// It needs a `tracker` that will persist the position in the log
    /// as it is being handled. You can use the
    /// [`Client::built_in_application_event_log_tracker`] if this call is
    /// used for the single main application handling this instance of the
    /// [`Client`]. Otherwise you should implement your own tracker.
    ///
    /// This handler will call `handle_fn` with ever event emitted by
    /// [`Client`], including transient ones. The caller should atomically
    /// handle each event it is interested in and ignore other ones.
    ///
    /// This method returns only when client is shutting down or on internal
    /// error, so typically should be called in a background task dedicated
    /// to handling events.
    pub async fn handle_events<F, R>(
        &self,
        tracker: fedimint_eventlog::DynEventLogTrimableTracker,
        handler_fn: F,
    ) -> anyhow::Result<()>
    where
        F: Fn(&mut DatabaseTransaction<NonCommittable>, EventLogEntry) -> R,
        R: Future<Output = anyhow::Result<()>>,
    {
        fedimint_eventlog::handle_trimable_events(
            self.db.clone(),
            tracker,
            self.log_event_added_rx.clone(),
            handler_fn,
        )
        .await
    }

    pub async fn get_event_log(
        &self,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        self.get_event_log_dbtx(&mut self.db.begin_transaction_nc().await, pos, limit)
            .await
    }

    pub async fn get_event_log_trimable(
        &self,
        pos: Option<EventLogTrimableId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        self.get_event_log_trimable_dbtx(&mut self.db.begin_transaction_nc().await, pos, limit)
            .await
    }

    pub async fn get_event_log_dbtx<Cap>(
        &self,
        dbtx: &mut DatabaseTransaction<'_, Cap>,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry>
    where
        Cap: Send,
    {
        dbtx.get_event_log(pos, limit).await
    }

    pub async fn get_event_log_trimable_dbtx<Cap>(
        &self,
        dbtx: &mut DatabaseTransaction<'_, Cap>,
        pos: Option<EventLogTrimableId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry>
    where
        Cap: Send,
    {
        dbtx.get_event_log_trimable(pos, limit).await
    }

    /// Register to receiver all new transient (unpersisted) events
    pub fn get_event_log_transient_receiver(&self) -> broadcast::Receiver<EventLogEntry> {
        self.log_event_added_transient_tx.subscribe()
    }

    /// Get a receiver that signals when new events are added to the event log
    pub fn log_event_added_rx(&self) -> watch::Receiver<()> {
        self.log_event_added_rx.clone()
    }

    pub(crate) async fn run_core_migrations(
        db_no_decoders: &Database,
    ) -> Result<(), anyhow::Error> {
        let mut dbtx = db_no_decoders.begin_transaction().await;
        apply_migrations_core_client_dbtx(&mut dbtx.to_ref_nc(), "fedimint-client".to_string())
            .await?;
        if is_running_in_test_env() {
            verify_client_db_integrity_dbtx(&mut dbtx.to_ref_nc()).await;
        }
        dbtx.commit_tx_result().await?;
        Ok(())
    }

    /// Returns the primary module, if any
    pub fn primary_module(&self) -> Option<(ModuleInstanceId, &DynClientModule)> {
        self.primary_module
            .map(|id| (id, self.modules.get_expect(id)))
    }

    /// Returns the primary module, panicking if none is available
    pub fn primary_module_for_btc(&self) -> (ModuleInstanceId, &DynClientModule) {
        self.primary_module().expect("No primary module available")
    }
}

#[apply(async_trait_maybe_send!)]
impl ClientContextIface for Client {
    fn get_module(&self, instance: ModuleInstanceId) -> &maybe_add_send_sync!(dyn IClientModule) {
        Client::get_module(self, instance)
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
        dbtx: &mut DatabaseTransaction<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        Client::finalize_and_submit_transaction_dbtx(self, dbtx, operation_id, tx_builder).await
    }

    async fn finalize_and_submit_transaction_inner(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
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

    async fn log_event_json(
        &self,
        dbtx: &mut DatabaseTransaction<'_, NonCommittable>,
        module_kind: Option<ModuleKind>,
        module_id: ModuleInstanceId,
        kind: EventKind,
        payload: serde_json::Value,
        persist: EventPersistence,
    ) {
        dbtx.ensure_global()
            .expect("Must be called with global dbtx");
        self.log_event_raw_dbtx(
            dbtx,
            kind,
            module_kind.map(|kind| (kind, module_id)),
            serde_json::to_vec(&payload).expect("Serialization can't fail"),
            persist,
        )
        .await;
    }

    fn event_log_transient_receiver(&self) -> broadcast::Receiver<EventLogEntry> {
        self.get_event_log_transient_receiver()
    }

    fn log_event_added_rx(&self) -> watch::Receiver<()> {
        Client::log_event_added_rx(self)
    }

    async fn get_event_log(&self, pos: Option<EventLogId>, limit: u64) -> Vec<PersistedLogEntry> {
        Client::get_event_log(self, pos, limit).await
    }

    async fn await_tx_accepted(&self, txid: TransactionId) -> Result<(), String> {
        Client::await_tx_accepted(self, txid).await
    }
}

// TODO: impl `Debug` for `Client` and derive here
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Client")
    }
}

pub fn client_decoders<'a>(
    registry: &ModuleInitRegistry<DynClientModuleInit>,
    module_kinds: impl Iterator<Item = (ModuleInstanceId, &'a ModuleKind)>,
) -> ModuleDecoderRegistry {
    let mut modules = BTreeMap::new();
    for (id, kind) in module_kinds {
        let Some(init) = registry.get(kind) else {
            debug!("Detected configuration for unsupported module id: {id}, kind: {kind}");
            continue;
        };

        modules.insert(
            id,
            (
                kind.clone(),
                IClientModuleInit::decoder(AsRef::<dyn IClientModuleInit + 'static>::as_ref(init)),
            ),
        );
    }
    ModuleDecoderRegistry::from(modules)
}
