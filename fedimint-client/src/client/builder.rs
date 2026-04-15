use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{bail, ensure};
use bitcoin::key::Secp256k1;
use fedimint_api_client::api::{DynGlobalApi, FederationApi};
use fedimint_api_client::{Endpoint, download_from_invite_code};
use fedimint_client_module::ModuleRecoveryStarted;
use fedimint_client_module::executor::ModuleExecutor;
use fedimint_client_module::module::init::ClientModuleInit;
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::module::{ClientModuleRegistry, FinalClientIface};
use fedimint_client_module::secret::{DeriveableSecretClientExt as _, get_default_client_secret};
use fedimint_client_module::transaction::TxSubmissionSmContext;
use fedimint_core::config::{ClientConfig, FederationId, ModuleInitRegistry};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::{
    Database, IReadDatabaseTransactionOpsTyped, IWriteDatabaseTransactionOpsTyped as _,
};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::task::TaskGroup;
use fedimint_core::util::{FmtCompactAnyhow as _, SafeUrl};
use fedimint_core::{NumPeers, PeerId, fedimint_build_code_version_env, maybe_add_send};
use fedimint_derive_secret::DerivableSecret;
use fedimint_eventlog::{
    DBTransactionEventLogExt as _, EventLogEntry, run_event_log_ordering_task,
};
use fedimint_logging::LOG_CLIENT;
use tokio::sync::{broadcast, watch};
use tracing::{debug, trace, warn};

use super::handle::ClientHandle;
use super::{Client, client_decoders};
use crate::db::{
    self, ApiSecretKey, ClientInitStateKey, ClientModuleRecovery, ClientModuleRecoveryState,
    ClientPreRootSecretHashKey, InitMode, InitState,
};
use crate::module_init::ClientModuleInitRegistry;

/// The type of root secret hashing
///
/// *Please read this documentation carefully if, especially if you're upgrading
/// downstream Fedimint client application.*
///
/// Internally, client will always hash-in federation id
/// to the root secret provided to the [`ClientBuilder`],
/// to ensure a different actual root secret is used for ever federation.
/// This makes reusing a single root secret for different federations
/// in a multi-federation client, perfectly fine, and frees the client
/// from worrying about `FederationId`.
///
/// However, in the past Fedimint applications (including `fedimint-cli`)
/// were doing the hashing-in of `FederationId` outside of `fedimint-client` as
/// well, which lead to effectively doing it twice, and pushed downloading of
/// the client config on join to application code, a sub-optimal API, especially
/// after joining federation needed to handle even more functionality.
///
/// To keep the interoperability of the seed phrases this double-derivation
/// is preserved, due to other architectural reason, `fedimint-client`
/// will now do the outer-derivation internally as well.
#[derive(Clone)]
pub enum RootSecret {
    /// Derive an extra round of federation-id to the secret, like
    /// Fedimint applications were doing manually in the past.
    ///
    /// **Note**: Applications MUST NOT do the derivation themselves anymore.
    StandardDoubleDerive(DerivableSecret),
    /// No double derivation
    ///
    /// This is useful for applications that for whatever reason do the
    /// double-derivation externally, or use a custom scheme.
    Custom(DerivableSecret),
}

impl RootSecret {
    fn to_inner(&self, federation_id: FederationId) -> DerivableSecret {
        match self {
            RootSecret::StandardDoubleDerive(derivable_secret) => {
                get_default_client_secret(derivable_secret, &federation_id)
            }
            RootSecret::Custom(derivable_secret) => derivable_secret.clone(),
        }
    }
}

/// Used to configure, assemble and build [`Client`]
pub struct ClientBuilder {
    module_inits: ClientModuleInitRegistry,
    stopped: bool,
    log_event_added_transient_tx: broadcast::Sender<EventLogEntry>,
}

impl ClientBuilder {
    pub(crate) fn new() -> Self {
        trace!(
            target: LOG_CLIENT,
            version = %fedimint_build_code_version_env!(),
            "Initializing fedimint client",
        );
        let (log_event_added_transient_tx, _log_event_added_transient_rx) =
            broadcast::channel(1024);

        ClientBuilder {
            module_inits: ModuleInitRegistry::new(),
            stopped: false,
            log_event_added_transient_tx,
        }
    }

    /// Replace module generator registry entirely
    pub fn with_module_inits(&mut self, module_inits: ClientModuleInitRegistry) {
        self.module_inits = module_inits;
    }

    /// Make module generator available when reading the config
    pub fn with_module<M: ClientModuleInit>(&mut self, module_init: M) {
        self.module_inits.attach(module_init);
    }

    pub fn stopped(&mut self) {
        self.stopped = true;
    }

    pub async fn load_existing_config(&self, db: &Database) -> anyhow::Result<ClientConfig> {
        let Some(config) = Client::get_config_from_db(db).await else {
            bail!("Client database not initialized")
        };

        Ok(config)
    }

    #[allow(clippy::too_many_arguments)]
    async fn init(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: ClientConfig,
        api_secret: Option<String>,
        init_mode: InitMode,
    ) -> anyhow::Result<ClientHandle> {
        if Client::is_initialized(&db_no_decoders).await {
            bail!("Client database already initialized")
        }

        Client::run_core_migrations(&db_no_decoders).await?;

        // Note: It's important all client initialization is performed as one big
        // transaction to avoid half-initialized client state.
        {
            debug!(target: LOG_CLIENT, "Initializing client database");
            let mut dbtx = db_no_decoders.begin_write_transaction().await;
            // Save config to DB
            dbtx.insert_new_entry(&crate::db::ClientConfigKey, &config)
                .await;
            dbtx.insert_entry(
                &ClientPreRootSecretHashKey,
                &pre_root_secret.derive_pre_root_secret_hash(),
            )
            .await;

            if let Some(api_secret) = api_secret.as_ref() {
                dbtx.insert_new_entry(&ApiSecretKey, api_secret).await;
            }

            let init_state = InitState::Pending(init_mode);
            dbtx.insert_entry(&ClientInitStateKey, &init_state).await;

            dbtx.commit_tx_result().await?;
        }

        let stopped = self.stopped;
        self.build(
            connectors,
            db_no_decoders,
            pre_root_secret,
            config,
            api_secret,
            stopped,
        )
        .await
    }

    pub async fn preview(
        self,
        connectors: Endpoint,
        invite_code: &InviteCode,
    ) -> anyhow::Result<ClientPreview> {
        let (config, _api) = download_from_invite_code(&connectors, invite_code).await?;

        Ok(ClientPreview {
            connectors,
            inner: self,
            config,
            api_secret: invite_code.api_secret(),
        })
    }

    /// Use [`Self::preview`] instead
    pub async fn preview_with_existing_config(
        self,
        connectors: Endpoint,
        config: ClientConfig,
        api_secret: Option<String>,
    ) -> anyhow::Result<ClientPreview> {
        Ok(ClientPreview {
            connectors,
            inner: self,
            config,
            api_secret,
        })
    }

    pub async fn open(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        Client::run_core_migrations(&db_no_decoders).await?;

        let Some(config) = Client::get_config_from_db(&db_no_decoders).await else {
            bail!("Client database not initialized")
        };

        let pre_root_secret = pre_root_secret.to_inner(config.calculate_federation_id());

        match db_no_decoders
            .begin_write_transaction()
            .await
            .get_value(&ClientPreRootSecretHashKey)
            .await
        {
            Some(secret_hash) => {
                ensure!(
                    pre_root_secret.derive_pre_root_secret_hash() == secret_hash,
                    "Secret hash does not match. Incorrect secret"
                );
            }
            _ => {
                debug!(target: LOG_CLIENT, "Backfilling secret hash");
                let mut dbtx = db_no_decoders.begin_write_transaction().await;
                dbtx.insert_entry(
                    &ClientPreRootSecretHashKey,
                    &pre_root_secret.derive_pre_root_secret_hash(),
                )
                .await;
                dbtx.commit_tx().await;
            }
        }

        let api_secret = Client::get_api_secret_from_db(&db_no_decoders).await;
        let stopped = self.stopped;

        let log_event_added_transient_tx = self.log_event_added_transient_tx.clone();
        let client = self
            .build_stopped(
                connectors,
                db_no_decoders,
                pre_root_secret,
                &config,
                api_secret,
                log_event_added_transient_tx,
            )
            .await?;
        if !stopped {
            client.start_executor();
        }
        Ok(client)
    }

    /// Build a [`Client`] and start the executor
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn build(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: ClientConfig,
        api_secret: Option<String>,
        stopped: bool,
    ) -> anyhow::Result<ClientHandle> {
        let log_event_added_transient_tx = self.log_event_added_transient_tx.clone();
        let client = self
            .build_stopped(
                connectors,
                db_no_decoders,
                pre_root_secret,
                &config,
                api_secret,
                log_event_added_transient_tx,
            )
            .await?;
        if !stopped {
            client.start_executor();
        }

        Ok(client)
    }

    // TODO: remove config argument
    /// Build a [`Client`] but do not start the executor
    async fn build_stopped(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: &ClientConfig,
        api_secret: Option<String>,
        log_event_added_transient_tx: broadcast::Sender<EventLogEntry>,
    ) -> anyhow::Result<ClientHandle> {
        debug!(
            target: LOG_CLIENT,
            version = %fedimint_build_code_version_env!(),
            "Building fedimint client",
        );
        let (log_event_added_tx, log_event_added_rx) = watch::channel(());
        let (log_ordering_wakeup_tx, log_ordering_wakeup_rx) = watch::channel(());

        let decoders = self.decoders(config);
        let config = Self::config_decoded(config, &decoders)?;
        let fed_id = config.calculate_federation_id();
        let db = db_no_decoders.with_decoders(decoders.clone());
        let peer_urls: BTreeMap<PeerId, SafeUrl> = config
            .global
            .api_endpoints
            .iter()
            .map(|(peer, endpoint)| (*peer, endpoint.url.clone()))
            .collect();
        let api: DynGlobalApi = FederationApi::new(connectors.clone(), peer_urls).into();

        let task_group = TaskGroup::new();

        let init_state = Self::load_init_state(&db).await;

        let mut module_recoveries: BTreeMap<
            ModuleInstanceId,
            Pin<Box<maybe_add_send!(dyn Future<Output = anyhow::Result<()>>)>>,
        > = BTreeMap::new();
        let mut module_recovery_progress_receivers: BTreeMap<
            ModuleInstanceId,
            watch::Receiver<RecoveryProgress>,
        > = BTreeMap::new();

        let final_client = FinalClientIface::default();

        let root_secret = Self::federation_root_secret(&pre_root_secret, &config);

        let modules = {
            let mut modules = ClientModuleRegistry::default();
            for (module_instance_id, module_config) in config.modules.clone() {
                let kind = module_config.kind().clone();
                let Some(module_init) = self.module_inits.get(&kind).cloned() else {
                    debug!(
                        target: LOG_CLIENT,
                        kind=%kind,
                        instance_id=%module_instance_id,
                        "Module kind of instance not found in module gens, skipping");
                    continue;
                };

                // since the exact logic of when to start recovery is a bit gnarly,
                // the recovery call is extracted here.
                let start_module_recover_fn = |progress: RecoveryProgress| {
                    let module_config = module_config.clone();
                    let num_peers = NumPeers::from(config.global.api_endpoints.len());
                    let db = db.clone();
                    let kind = kind.clone();
                    let api = api.clone();
                    let root_secret = root_secret.clone();
                    let final_client = final_client.clone();
                    let (progress_tx, progress_rx) = tokio::sync::watch::channel(progress);
                    let task_group = task_group.clone();
                    let module_init = module_init.clone();
                    (
                        Box::pin(async move {
                            module_init
                                    .recover(
                                        final_client.clone(),
                                        fed_id,
                                        num_peers,
                                        module_config.clone(),
                                        db.clone(),
                                        module_instance_id,
                                        root_secret.derive_module_secret(module_instance_id),
                                        api.clone(),
                                        progress_tx,
                                        task_group,
                                    )
                                    .await
                                    .inspect_err(|err| {
                                        warn!(
                                            target: LOG_CLIENT,
                                            module_id = module_instance_id, %kind, err = %err.fmt_compact_anyhow(), "Module failed to recover"
                                        );
                                    })
                        }),
                        progress_rx,
                    )
                };

                let recovery = if init_state.does_require_recovery() {
                    let existing_recovery_state = db
                        .begin_write_transaction()
                        .await
                        .get_value(&ClientModuleRecovery { module_instance_id })
                        .await;
                    match existing_recovery_state {
                        Some(module_recovery_state) => {
                            if module_recovery_state.is_done() {
                                debug!(
                                    id = %module_instance_id,
                                    %kind, "Module recovery already complete"
                                );
                                None
                            } else {
                                debug!(
                                    id = %module_instance_id,
                                    %kind,
                                    progress = %module_recovery_state.progress,
                                    "Starting module recovery with an existing progress"
                                );
                                Some(start_module_recover_fn(module_recovery_state.progress))
                            }
                        }
                        _ => {
                            let progress = RecoveryProgress::none();
                            let mut dbtx = db.begin_write_transaction().await;
                            dbtx.log_event(
                                log_ordering_wakeup_tx.clone(),
                                None,
                                ModuleRecoveryStarted::new(module_instance_id),
                            )
                            .await;
                            dbtx.insert_entry(
                                &ClientModuleRecovery { module_instance_id },
                                &ClientModuleRecoveryState { progress },
                            )
                            .await;

                            dbtx.commit_tx().await;

                            debug!(
                                id = %module_instance_id,
                                %kind, "Starting new module recovery"
                            );
                            Some(start_module_recover_fn(progress))
                        }
                    }
                } else {
                    None
                };

                match recovery {
                    Some((recovery, recovery_progress_rx)) => {
                        module_recoveries.insert(module_instance_id, recovery);
                        module_recovery_progress_receivers
                            .insert(module_instance_id, recovery_progress_rx);
                    }
                    _ => {
                        let module = module_init
                            .init(
                                final_client.clone(),
                                fed_id,
                                config.global.api_endpoints.len(),
                                module_config,
                                db.clone(),
                                module_instance_id,
                                // This is a divergence from the legacy client, where the child
                                // secret keys were derived using
                                // *module kind*-specific derivation paths.
                                // Since the new client has to support multiple, segregated modules
                                // of the same kind we have to use
                                // the instance id instead.
                                root_secret.derive_module_secret(module_instance_id),
                                api.clone(),
                                task_group.clone(),
                                connectors.clone(),
                            )
                            .await?;

                        modules.register_module(module_instance_id, kind, module);
                    }
                }
            }
            modules
        };

        if init_state.is_pending() && module_recoveries.is_empty() {
            let mut dbtx = db.begin_write_transaction().await;
            dbtx.insert_entry(&ClientInitStateKey, &init_state.into_complete())
                .await;
            dbtx.commit_tx().await;
        }

        let primary_module = modules
            .iter_modules()
            .find(|(_id, _kind, module)| module.supports_being_primary())
            .map(|(id, _kind, _module)| id);

        let recovery_receiver_init_val = module_recovery_progress_receivers
            .iter()
            .map(|(module_instance_id, rx)| (*module_instance_id, *rx.borrow()))
            .collect::<BTreeMap<_, _>>();
        let (client_recovery_progress_sender, client_recovery_progress_receiver) =
            watch::channel(recovery_receiver_init_val);

        let tx_submission_executor = ModuleExecutor::new(
            db.clone(),
            TxSubmissionSmContext {
                api: api.clone(),
                decoders: decoders.clone(),
                client: final_client.clone(),
            },
            task_group.clone(),
        );

        let client_inner = Arc::new(Client {
            config: tokio::sync::RwLock::new(config.clone()),
            api_secret,
            decoders,
            db: db.clone(),
            connectors,
            federation_id: fed_id,
            federation_config_meta: config.global.meta,
            primary_module,
            modules,
            log_ordering_wakeup_tx,
            log_event_added_rx,
            log_event_added_transient_tx: log_event_added_transient_tx.clone(),
            tx_submission_executor,
            api,
            secp_ctx: Secp256k1::new(),
            task_group,
            client_recovery_progress_receiver,
        });

        client_inner
            .task_group
            .spawn_cancellable("event log ordering task", {
                async move {
                    run_event_log_ordering_task(
                        db.clone(),
                        log_ordering_wakeup_rx,
                        log_event_added_tx,
                        log_event_added_transient_tx,
                    )
                    .await
                }
            });

        let client_iface = std::sync::Arc::<Client>::downgrade(&client_inner);

        let client_arc = ClientHandle::new(client_inner);

        final_client.set(client_iface.clone());

        client_arc.tx_submission_executor.start().await;

        // Module `start` is called *after* `final_client.set` so that any
        // per-module executors spawned here can safely resolve the weak
        // client reference from their transition contexts.
        for (_, _, module) in client_arc.modules.iter_modules() {
            module.start().await;
        }

        if !module_recoveries.is_empty() {
            client_arc.spawn_module_recoveries_task(
                client_recovery_progress_sender,
                module_recoveries,
                module_recovery_progress_receivers,
            );
        }

        Ok(client_arc)
    }

    async fn load_init_state(db: &Database) -> InitState {
        let mut dbtx = db.begin_write_transaction().await;
        dbtx.get_value(&ClientInitStateKey)
            .await
            .unwrap_or_else(|| {
                // could be turned in a hard error in the future, but for now
                // no need to break backward compat.
                warn!(
                    target: LOG_CLIENT,
                    "Client missing ClientRequiresRecovery: assuming complete"
                );
                db::InitState::Complete(db::InitModeComplete::Fresh)
            })
    }

    fn decoders(&self, config: &ClientConfig) -> ModuleDecoderRegistry {
        let decoders = client_decoders(
            &self.module_inits,
            config
                .modules
                .iter()
                .map(|(module_instance, module_config)| (*module_instance, module_config.kind())),
        );

        decoders
    }

    fn config_decoded(
        config: &ClientConfig,
        decoders: &ModuleDecoderRegistry,
    ) -> Result<ClientConfig, fedimint_core::encoding::DecodeError> {
        config.clone().redecode_raw(decoders)
    }

    /// Re-derive client's `root_secret` using the federation ID. This
    /// eliminates the possibility of having the same client `root_secret`
    /// across multiple federations.
    fn federation_root_secret(
        pre_root_secret: &DerivableSecret,
        config: &ClientConfig,
    ) -> DerivableSecret {
        pre_root_secret.federation_key(&config.global.calculate_federation_id())
    }

    /// Register to receiver all new transient (unpersisted) events
    pub fn get_event_log_transient_receiver(&self) -> broadcast::Receiver<EventLogEntry> {
        self.log_event_added_transient_tx.subscribe()
    }
}

/// An intermediate step before Client joining or recovering
///
/// Meant to support showing user some initial information about the Federation
/// before actually joining.
pub struct ClientPreview {
    inner: ClientBuilder,
    config: ClientConfig,
    connectors: Endpoint,
    api_secret: Option<String>,
}

impl ClientPreview {
    /// Get the config
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Join a new Federation
    ///
    /// When a user wants to connect to a new federation this function fetches
    /// the federation config and initializes the client database. If a user
    /// already joined the federation in the past and has a preexisting database
    /// use [`ClientBuilder::open`] instead.
    ///
    /// **Warning**: Calling `join` with a `root_secret` key that was used
    /// previous to `join` a Federation will lead to all sorts of malfunctions
    /// including likely loss of funds.
    ///
    /// This should be generally called only if the `root_secret` key is known
    /// not to have been used before (e.g. just randomly generated). For keys
    /// that might have been previous used (e.g. provided by the user),
    /// it's safer to call [`Self::recover`] which will attempt to recover
    /// client module states for the Federation.
    ///
    /// A typical "join federation" flow would look as follows:
    /// ```no_run
    /// # use std::str::FromStr;
    /// # use fedimint_core::invite_code::InviteCode;
    /// # use fedimint_core::config::ClientConfig;
    /// # use fedimint_derive_secret::DerivableSecret;
    /// # use fedimint_client::{Client, ClientBuilder, RootSecret};
    /// # use fedimint_api_client::Endpoint;
    /// # use fedimint_core::db::Database;
    /// # use fedimint_core::config::META_FEDERATION_NAME_KEY;
    /// #
    /// # #[tokio::main]
    /// # async fn main() -> anyhow::Result<()> {
    /// # let root_secret: DerivableSecret = unimplemented!();
    /// // Create a root secret, e.g. via fedimint-bip39, see also:
    /// // https://github.com/fedimint/fedimint/blob/master/docs/secret_derivation.md
    /// // let root_secret = …;
    ///
    /// // Get invite code from user
    /// let invite_code = InviteCode::from_str("fed11qgqpw9thwvaz7te3xgmjuvpwxqhrzw3jxumrvvf0qqqjpetvlg8glnpvzcufhffgzhv8m75f7y34ryk7suamh8x7zetly8h0v9v0rm")
    ///     .expect("Invalid invite code");
    ///
    /// // Tell the user the federation name, bitcoin network
    /// // (e.g. from wallet module config), and other details
    /// // that are typically contained in the federation's
    /// // meta fields.
    ///
    /// // let network = config.get_first_module_by_kind::<WalletClientConfig>("wallet")
    /// //     .expect("Module not found")
    /// //     .network;
    ///
    /// // Open the client's database, using the federation ID
    /// // as the DB name is a common pattern:
    ///
    /// // let db_path = format!("./path/to/db/{}", config.federation_id());
    /// // let db = RedbDatabase::open(db_path).expect("error opening DB");
    /// # let db: Database = unimplemented!();
    /// # let connectors: Endpoint = unimplemented!();
    ///
    /// let preview = Client::builder().await
    ///     // Mount the modules the client should support:
    ///     // .with_module(LightningClientInit)
    ///     // .with_module(MintClientInit)
    ///     // .with_module(WalletClientInit::default())
    ///      .expect("Error building client")
    ///      .preview(connectors, &invite_code).await?;
    ///
    /// println!(
    ///     "The federation name is: {}",
    ///     preview.config().meta::<String>(META_FEDERATION_NAME_KEY)
    ///         .expect("Could not decode name field")
    ///         .expect("Name isn't set")
    /// );
    ///
    /// let client = preview
    ///     .join(db, RootSecret::StandardDoubleDerive(root_secret))
    ///     .await
    ///     .expect("Error joining federation");
    /// # Ok(())
    /// # }
    /// ```
    pub async fn join(
        self,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let pre_root_secret = pre_root_secret.to_inner(self.config.calculate_federation_id());

        let client = self
            .inner
            .init(
                self.connectors,
                db_no_decoders,
                pre_root_secret,
                self.config,
                self.api_secret,
                InitMode::Fresh,
            )
            .await?;

        Ok(client)
    }

    /// Join a (possibly) previous joined Federation
    ///
    /// Unlike [`Self::join`], `recover` will run client module
    /// recovery for each client module attempting to recover any previous
    /// module state.
    ///
    /// Recovery process takes time during which each recovering client module
    /// will not be available for use.
    ///
    /// Calling `recovery` with a `root_secret` that was not actually previous
    /// used in a given Federation is safe.
    pub async fn recover(
        self,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let pre_root_secret = pre_root_secret.to_inner(self.config.calculate_federation_id());

        let client = self
            .inner
            .init(
                self.connectors,
                db_no_decoders,
                pre_root_secret,
                self.config,
                self.api_secret,
                InitMode::Recover,
            )
            .await?;

        Ok(client)
    }
}
