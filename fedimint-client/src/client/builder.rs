use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::bail;
use bitcoin::key::Secp256k1;
use fedimint_api_client::api::{DynGlobalApi, FederationApi};
use fedimint_api_client::{Endpoint, download_from_invite_code, wire};
use fedimint_client_module::ModuleRecoveryStarted;
use fedimint_client_module::executor::ModuleExecutor;
use fedimint_client_module::module::init::ClientModuleInit;
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::module::{ClientModule, FinalClientIface};
use fedimint_client_module::secret::{DeriveableSecretClientExt as _, get_default_client_secret};
use fedimint_client_module::transaction::TxSubmissionSmContext;
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::CommonModuleInit;
use fedimint_core::task::TaskGroup;
use fedimint_core::util::{FmtCompactAnyhow as _, SafeUrl};
use fedimint_core::{NumPeers, PeerId, fedimint_build_code_version_env, maybe_add_send};
use fedimint_derive_secret::DerivableSecret;
use fedimint_gwv2_client::{GatewayClientInitV2, IGatewayClientV2};
use fedimint_lnv2_client::LightningClientInit;
use fedimint_logging::LOG_CLIENT;
use fedimint_mintv2_client::MintClientInit;
use fedimint_redb::Database;
use fedimint_walletv2_client::WalletClientInit;
use tokio::sync::watch;
use tracing::{debug, trace, warn};

use super::handle::ClientHandle;
use super::{Client, LnFlavor};
use crate::db::{
    self, CLIENT_CONFIG, CLIENT_INIT_STATE, CLIENT_MODULE_RECOVERY, ClientModuleRecovery,
    ClientModuleRecoveryState, InitMode, InitState,
};

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

/// Choice of LN-module client flavor mounted on the built [`Client`].
///
/// Regular applications get `Regular`; the gateway daemon supplies `Gateway`
/// via [`ClientBuilder::with_gateway_ln`]. The two are mutually exclusive
/// because the federation-side lnv2 module at instance id `1` accepts only
/// one client-side wrapper.
pub enum LnInit {
    Regular(LightningClientInit),
    Gateway(GatewayClientInitV2),
}

/// Used to configure, assemble and build [`Client`]
pub struct ClientBuilder {
    mint_init: MintClientInit,
    wallet_init: WalletClientInit,
    ln_init: LnInit,
    stopped: bool,
}

impl ClientBuilder {
    pub(crate) fn new() -> Self {
        trace!(
            target: LOG_CLIENT,
            version = %fedimint_build_code_version_env!(),
            "Initializing fedimint client",
        );

        ClientBuilder {
            mint_init: MintClientInit,
            wallet_init: WalletClientInit,
            ln_init: LnInit::Regular(LightningClientInit::default()),
            stopped: false,
        }
    }

    /// Configure the regular lightning client flavor with a specific init
    /// (e.g. to override the gateway connection).
    pub fn with_lightning_init(&mut self, init: LightningClientInit) {
        self.ln_init = LnInit::Regular(init);
    }

    /// Mount the gateway lightning flavor. Used by the gateway daemon in
    /// place of the regular lightning module.
    pub fn with_gateway_ln(&mut self, gateway: Arc<dyn IGatewayClientV2>) {
        self.ln_init = LnInit::Gateway(GatewayClientInitV2 { gateway });
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

    async fn init(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: ClientConfig,
        init_mode: InitMode,
    ) -> anyhow::Result<ClientHandle> {
        if Client::is_initialized(&db_no_decoders).await {
            bail!("Client database already initialized")
        }

        {
            debug!(target: LOG_CLIENT, "Initializing client database");
            let dbtx = db_no_decoders.begin_write().await;
            let tx = dbtx.as_ref();
            tx.insert(&CLIENT_CONFIG, &(), &config);

            let init_state = InitState::Pending(init_mode);
            tx.insert(&CLIENT_INIT_STATE, &(), &init_state);

            dbtx.commit().await;
        }

        let stopped = self.stopped;
        self.build(connectors, db_no_decoders, pre_root_secret, config, stopped)
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
        })
    }

    /// Use [`Self::preview`] instead
    pub async fn preview_with_existing_config(
        self,
        connectors: Endpoint,
        config: ClientConfig,
    ) -> anyhow::Result<ClientPreview> {
        Ok(ClientPreview {
            connectors,
            inner: self,
            config,
        })
    }

    pub async fn open(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let Some(config) = Client::get_config_from_db(&db_no_decoders).await else {
            bail!("Client database not initialized")
        };

        let pre_root_secret = pre_root_secret.to_inner(config.calculate_federation_id());

        let stopped = self.stopped;

        let client = self
            .build_stopped(connectors, db_no_decoders, pre_root_secret, &config)
            .await?;
        if !stopped {
            client.start_executor();
        }
        Ok(client)
    }

    /// Build a [`Client`] and start the executor
    pub(crate) async fn build(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: ClientConfig,
        stopped: bool,
    ) -> anyhow::Result<ClientHandle> {
        let client = self
            .build_stopped(connectors, db_no_decoders, pre_root_secret, &config)
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
    ) -> anyhow::Result<ClientHandle> {
        debug!(
            target: LOG_CLIENT,
            version = %fedimint_build_code_version_env!(),
            "Building fedimint client",
        );
        let (log_event_added_tx, _) = watch::channel(());

        let config = config.clone();
        let fed_id = config.calculate_federation_id();
        let db = db_no_decoders;
        let peer_urls: BTreeMap<PeerId, SafeUrl> = config
            .global
            .api_endpoints
            .iter()
            .map(|(peer, endpoint)| (*peer, endpoint.url.clone()))
            .collect();
        let api: DynGlobalApi = FederationApi::new(connectors.clone(), peer_urls).into();

        let task_group = TaskGroup::new();

        let init_state = Self::load_init_state(&db).await;

        let final_client = FinalClientIface::default();

        let root_secret = Self::federation_root_secret(&pre_root_secret, &config);
        let num_peers = NumPeers::from(config.global.api_endpoints.len());

        let mut module_recoveries: BTreeMap<
            ModuleInstanceId,
            Pin<Box<maybe_add_send!(dyn Future<Output = anyhow::Result<()>>)>>,
        > = BTreeMap::new();
        let mut module_recovery_progress_receivers: BTreeMap<
            ModuleInstanceId,
            watch::Receiver<RecoveryProgress>,
        > = BTreeMap::new();

        let mint = init_or_recover(
            &self.mint_init,
            wire::MINT_INSTANCE_ID,
            "mintv2",
            &config,
            &db,
            &api,
            &connectors,
            &task_group,
            &root_secret,
            final_client.clone(),
            fed_id,
            num_peers,
            &init_state,
            &log_event_added_tx,
            &mut module_recoveries,
            &mut module_recovery_progress_receivers,
        )
        .await?;

        let wallet = init_or_recover(
            &self.wallet_init,
            wire::WALLET_INSTANCE_ID,
            "walletv2",
            &config,
            &db,
            &api,
            &connectors,
            &task_group,
            &root_secret,
            final_client.clone(),
            fed_id,
            num_peers,
            &init_state,
            &log_event_added_tx,
            &mut module_recoveries,
            &mut module_recovery_progress_receivers,
        )
        .await?;

        let ln = match self.ln_init {
            LnInit::Regular(init) => LnFlavor::Regular(
                init_or_recover(
                    &init,
                    wire::LN_INSTANCE_ID,
                    "lnv2",
                    &config,
                    &db,
                    &api,
                    &connectors,
                    &task_group,
                    &root_secret,
                    final_client.clone(),
                    fed_id,
                    num_peers,
                    &init_state,
                    &log_event_added_tx,
                    &mut module_recoveries,
                    &mut module_recovery_progress_receivers,
                )
                .await?,
            ),
            LnInit::Gateway(init) => LnFlavor::Gateway(
                init_or_recover(
                    &init,
                    wire::LN_INSTANCE_ID,
                    "lnv2",
                    &config,
                    &db,
                    &api,
                    &connectors,
                    &task_group,
                    &root_secret,
                    final_client.clone(),
                    fed_id,
                    num_peers,
                    &init_state,
                    &log_event_added_tx,
                    &mut module_recoveries,
                    &mut module_recovery_progress_receivers,
                )
                .await?,
            ),
        };

        if init_state.is_pending() && module_recoveries.is_empty() {
            let dbtx = db.begin_write().await;
            dbtx.as_ref()
                .insert(&CLIENT_INIT_STATE, &(), &init_state.into_complete());
            dbtx.commit().await;
        }

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
                client: final_client.clone(),
            },
            task_group.clone(),
        );

        let client_inner = Arc::new(Client {
            config: tokio::sync::RwLock::new(config.clone()),
            db: db.clone(),
            connectors,
            federation_id: fed_id,
            federation_config_meta: config.global.meta,
            mint,
            wallet,
            ln,
            log_event_added_tx,
            tx_submission_executor,
            api,
            secp_ctx: Secp256k1::new(),
            task_group,
            client_recovery_progress_receiver,
        });

        let client_iface = std::sync::Arc::<Client>::downgrade(&client_inner);

        let client_arc = ClientHandle::new(client_inner);

        final_client.set(client_iface.clone());

        client_arc.tx_submission_executor.start().await;

        // Module `start` is called *after* `final_client.set` so that any
        // per-module executors spawned here can safely resolve the weak
        // client reference from their transition contexts.
        client_arc.mint.start().await;
        client_arc.wallet.start().await;
        client_arc.ln.start().await;

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
        db.begin_read()
            .await
            .as_ref()
            .get(&CLIENT_INIT_STATE, &())
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

    /// Re-derive client's `root_secret` using the federation ID. This
    /// eliminates the possibility of having the same client `root_secret`
    /// across multiple federations.
    fn federation_root_secret(
        pre_root_secret: &DerivableSecret,
        config: &ClientConfig,
    ) -> DerivableSecret {
        pre_root_secret.federation_key(&config.global.calculate_federation_id())
    }
}

/// Run init (or spawn recovery if the database indicates recovery is
/// required). Returns an `Arc` of the typed module on success; on recovery
/// the future is stashed into `module_recoveries` and this returns — but we
/// still need a module instance to store in `Client`, so recovery paths
/// currently require running init after recovery finishes. For now we always
/// run init; recovery is scheduled to run in parallel on the task group.
#[allow(clippy::too_many_arguments)]
async fn init_or_recover<I: ClientModuleInit>(
    init: &I,
    module_instance_id: ModuleInstanceId,
    kind_str: &'static str,
    config: &ClientConfig,
    db: &Database,
    api: &DynGlobalApi,
    connectors: &Endpoint,
    task_group: &TaskGroup,
    root_secret: &DerivableSecret,
    final_client: FinalClientIface,
    fed_id: FederationId,
    num_peers: NumPeers,
    init_state: &InitState,
    log_event_added_tx: &watch::Sender<()>,
    module_recoveries: &mut BTreeMap<
        ModuleInstanceId,
        Pin<Box<maybe_add_send!(dyn Future<Output = anyhow::Result<()>>)>>,
    >,
    module_recovery_progress_receivers: &mut BTreeMap<
        ModuleInstanceId,
        watch::Receiver<RecoveryProgress>,
    >,
) -> anyhow::Result<Arc<<I as ClientModuleInit>::Module>> {
    let module_config = config
        .modules
        .get(&module_instance_id)
        .cloned()
        .unwrap_or_else(|| {
            panic!("Module config for {kind_str} missing at instance {module_instance_id}")
        });

    let typed_cfg: <<I as fedimint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig =
        module_config.cast()?;

    if init_state.does_require_recovery() {
        schedule_recovery(
            init,
            module_instance_id,
            kind_str,
            typed_cfg.clone(),
            db,
            api,
            task_group,
            root_secret,
            final_client.clone(),
            fed_id,
            num_peers,
            log_event_added_tx,
            module_recoveries,
            module_recovery_progress_receivers,
        )
        .await?;
    }

    let module_db = db.isolate(format!("module-{module_instance_id}"));
    let args = fedimint_client_module::module::init::ClientModuleInitArgs {
        federation_id: fed_id,
        peer_num: num_peers.total(),
        cfg: typed_cfg.clone(),
        db: module_db.clone(),
        module_root_secret: root_secret.derive_module_secret(module_instance_id),
        api: api.clone(),
        module_api: api.with_module(module_instance_id),
        context: fedimint_client_module::module::ClientContext::new(
            final_client,
            module_instance_id,
            module_db,
        ),
        task_group: task_group.clone(),
        connector_registry: connectors.clone(),
    };

    Ok(Arc::new(<I as ClientModuleInit>::init(init, &args).await?))
}

#[allow(clippy::too_many_arguments)]
async fn schedule_recovery<I: ClientModuleInit>(
    init: &I,
    module_instance_id: ModuleInstanceId,
    kind_str: &'static str,
    cfg: <<I as fedimint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig,
    db: &Database,
    api: &DynGlobalApi,
    task_group: &TaskGroup,
    root_secret: &DerivableSecret,
    final_client: FinalClientIface,
    fed_id: FederationId,
    num_peers: NumPeers,
    log_event_added_tx: &watch::Sender<()>,
    module_recoveries: &mut BTreeMap<
        ModuleInstanceId,
        Pin<Box<maybe_add_send!(dyn Future<Output = anyhow::Result<()>>)>>,
    >,
    module_recovery_progress_receivers: &mut BTreeMap<
        ModuleInstanceId,
        watch::Receiver<RecoveryProgress>,
    >,
) -> anyhow::Result<()>
where
    I: Clone,
{
    let existing_recovery_state = db.begin_read().await.as_ref().get(
        &CLIENT_MODULE_RECOVERY,
        &ClientModuleRecovery { module_instance_id },
    );

    let progress = match existing_recovery_state {
        Some(state) if state.is_done() => {
            debug!(id = %module_instance_id, kind = %kind_str, "Module recovery already complete");
            return Ok(());
        }
        Some(state) => state.progress,
        None => {
            let progress = RecoveryProgress::none();
            let dbtx = db.begin_write().await;
            let tx = dbtx.as_ref();
            fedimint_eventlog::log_event(
                &tx,
                log_event_added_tx.clone(),
                None,
                None,
                ModuleRecoveryStarted::new(module_instance_id),
            );
            tx.insert(
                &CLIENT_MODULE_RECOVERY,
                &ClientModuleRecovery { module_instance_id },
                &ClientModuleRecoveryState { progress },
            );
            dbtx.commit().await;
            progress
        }
    };

    let (progress_tx, progress_rx) = tokio::sync::watch::channel(progress);
    let module_db = db.isolate(format!("module-{module_instance_id}"));
    let module_api = api.with_module(module_instance_id);
    let init_clone = init.clone();
    let api_clone = api.clone();
    let task_group_clone = task_group.clone();
    let root_secret_clone = root_secret.clone();
    let final_client_clone = final_client.clone();

    let recover_fut = Box::pin(async move {
        let args = fedimint_client_module::module::init::ClientModuleRecoverArgs {
            federation_id: fed_id,
            num_peers,
            cfg: cfg.clone(),
            db: module_db.clone(),
            module_root_secret: root_secret_clone.derive_module_secret(module_instance_id),
            api: api_clone,
            module_api,
            context: fedimint_client_module::module::ClientContext::new(
                final_client_clone,
                module_instance_id,
                module_db,
            ),
            progress_tx,
            task_group: task_group_clone,
        };
        <I as ClientModuleInit>::recover(&init_clone, &args)
            .await
            .inspect_err(|err| {
                warn!(
                    target: LOG_CLIENT,
                    module_id = module_instance_id, kind = %kind_str,
                    err = %err.fmt_compact_anyhow(), "Module failed to recover"
                );
            })
    });

    module_recoveries.insert(module_instance_id, recover_fut);
    module_recovery_progress_receivers.insert(module_instance_id, progress_rx);
    Ok(())
}

/// An intermediate step before Client joining or recovering
///
/// Meant to support showing user some initial information about the Federation
/// before actually joining.
pub struct ClientPreview {
    inner: ClientBuilder,
    config: ClientConfig,
    connectors: Endpoint,
}

impl ClientPreview {
    /// Get the config
    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// Join a new Federation
    pub async fn join(
        self,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let pre_root_secret = pre_root_secret.to_inner(self.config.calculate_federation_id());

        self.inner
            .init(
                self.connectors,
                db_no_decoders,
                pre_root_secret,
                self.config,
                InitMode::Fresh,
            )
            .await
    }

    /// Join a (possibly) previous joined Federation, with module recovery.
    pub async fn recover(
        self,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let pre_root_secret = pre_root_secret.to_inner(self.config.calculate_federation_id());

        self.inner
            .init(
                self.connectors,
                db_no_decoders,
                pre_root_secret,
                self.config,
                InitMode::Recover,
            )
            .await
    }
}
