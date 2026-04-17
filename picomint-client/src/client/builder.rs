use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::bail;
use bitcoin::key::Secp256k1;
use picomint_api_client::api::{ApiScope, FederationApi};
use picomint_api_client::config::ConsensusConfig;
use picomint_api_client::{Endpoint, download_from_invite_code};
use picomint_client_module::ModuleRecoveryStarted;
use picomint_client_module::executor::ModuleExecutor;
use picomint_client_module::module::init::ClientModuleInit;
use picomint_client_module::module::recovery::RecoveryProgress;
use picomint_client_module::module::{ClientModule, FinalClientIface};
use picomint_client_module::secret::{DeriveableSecretClientExt as _, get_default_client_secret};
use picomint_client_module::transaction::TxSubmissionSmContext;
use picomint_core::config::FederationId;
use picomint_core::core::ModuleKind;
use picomint_core::invite_code::InviteCode;
use picomint_core::module::CommonModuleInit;
use picomint_core::task::TaskGroup;
use picomint_core::{NumPeers, PeerId};
use picomint_derive_secret::DerivableSecret;
use picomint_gw_client::{GatewayClientInitV2, IGatewayClientV2};
use picomint_ln_client::LightningClientInit;
use picomint_logging::LOG_CLIENT;
use picomint_mint_client::MintClientInit;
use picomint_redb::Database;
use picomint_wallet_client::WalletClientInit;
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
/// downstream Picomint client application.*
///
/// Internally, client will always hash-in federation id
/// to the root secret provided to the [`ClientBuilder`],
/// to ensure a different actual root secret is used for ever federation.
/// This makes reusing a single root secret for different federations
/// in a multi-federation client, perfectly fine, and frees the client
/// from worrying about `FederationId`.
///
/// However, in the past Picomint applications (including `picomint-cli`)
/// were doing the hashing-in of `FederationId` outside of `picomint-client` as
/// well, which lead to effectively doing it twice, and pushed downloading of
/// the client config on join to application code, a sub-optimal API, especially
/// after joining federation needed to handle even more functionality.
///
/// To keep the interoperability of the seed phrases this double-derivation
/// is preserved, due to other architectural reason, `picomint-client`
/// will now do the outer-derivation internally as well.
#[derive(Clone)]
pub enum RootSecret {
    /// Derive an extra round of federation-id to the secret, like
    /// Picomint applications were doing manually in the past.
    ///
    /// **Note**: Applications MUST NOT do the derivation themselves anymore.
    StandardDoubleDerive(DerivableSecret),
}

impl RootSecret {
    fn to_inner(&self, federation_id: FederationId) -> DerivableSecret {
        match self {
            RootSecret::StandardDoubleDerive(derivable_secret) => {
                get_default_client_secret(derivable_secret, &federation_id)
            }
        }
    }
}

/// Choice of LN-module client flavor mounted on the built [`Client`].
///
/// Regular applications get `Regular`; the gateway daemon supplies `Gateway`
/// via [`ClientBuilder::with_gateway_ln`]. The two are mutually exclusive
/// because the federation-side ln module at instance id `1` accepts only
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
            version = %env!("CARGO_PKG_VERSION"),
            "Initializing picomint client",
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

    pub async fn load_existing_config(&self, db: &Database) -> anyhow::Result<ConsensusConfig> {
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
        config: ConsensusConfig,
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
        config: ConsensusConfig,
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
        config: ConsensusConfig,
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
        config: &ConsensusConfig,
    ) -> anyhow::Result<ClientHandle> {
        debug!(
            target: LOG_CLIENT,
            version = %env!("CARGO_PKG_VERSION"),
            "Building picomint client",
        );
        let (log_event_added_tx, _) = watch::channel(());

        let config = config.clone();
        let fed_id = config.calculate_federation_id();
        let db = db_no_decoders;
        let peer_node_ids: BTreeMap<PeerId, iroh_base::PublicKey> = config
            .iroh_endpoints
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.node_id))
            .collect();
        let api: FederationApi = FederationApi::new(connectors.clone(), peer_node_ids).into();

        let task_group = TaskGroup::new();

        let init_state = Self::load_init_state(&db).await;

        let final_client = FinalClientIface::default();

        let root_secret = Self::federation_root_secret(&pre_root_secret, &config);
        let num_peers = NumPeers::from(config.iroh_endpoints.len());

        let mut module_recoveries: BTreeMap<
            ModuleKind,
            Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
        > = BTreeMap::new();
        let mut module_recovery_progress_receivers: BTreeMap<
            ModuleKind,
            watch::Receiver<RecoveryProgress>,
        > = BTreeMap::new();

        let mint = init_or_recover(
            &self.mint_init,
            ModuleKind::Mint,
            ApiScope::Mint,
            config.mint.clone(),
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
            ModuleKind::Wallet,
            ApiScope::Wallet,
            config.wallet.clone(),
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
                    ModuleKind::Ln,
                    ApiScope::Ln,
                    config.ln.clone(),
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
                    ModuleKind::Ln,
                    ApiScope::Ln,
                    config.ln.clone(),
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
            .map(|(kind, rx)| (*kind, *rx.borrow()))
            .collect::<BTreeMap<_, _>>();
        let (client_recovery_progress_sender, client_recovery_progress_receiver) =
            watch::channel(recovery_receiver_init_val);

        let tx_submission_executor = ModuleExecutor::new(
            db.clone(),
            TxSubmissionSmContext {
                api: api.clone(),
                log_event_added_tx: log_event_added_tx.clone(),
            },
            task_group.clone(),
        );

        let client_inner = Arc::new(Client {
            config: tokio::sync::RwLock::new(config.clone()),
            db: db.clone(),
            connectors,
            federation_id: fed_id,
            federation_config_meta: config.meta,
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
        config: &ConsensusConfig,
    ) -> DerivableSecret {
        pre_root_secret.federation_key(&config.calculate_federation_id())
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
    kind: ModuleKind,
    api_scope: ApiScope,
    typed_cfg: <<I as picomint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig,
    full_config: &ConsensusConfig,
    db: &Database,
    api: &FederationApi,
    connectors: &Endpoint,
    task_group: &TaskGroup,
    root_secret: &DerivableSecret,
    final_client: FinalClientIface,
    fed_id: FederationId,
    num_peers: NumPeers,
    init_state: &InitState,
    log_event_added_tx: &watch::Sender<()>,
    module_recoveries: &mut BTreeMap<
        ModuleKind,
        Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
    >,
    module_recovery_progress_receivers: &mut BTreeMap<
        ModuleKind,
        watch::Receiver<RecoveryProgress>,
    >,
) -> anyhow::Result<Arc<<I as ClientModuleInit>::Module>> {
    if init_state.does_require_recovery() {
        schedule_recovery(
            init,
            kind,
            api_scope,
            typed_cfg.clone(),
            full_config,
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

    let module_db = db.isolate(format!("module-{kind}"));
    let args = picomint_client_module::module::init::ClientModuleInitArgs {
        federation_id: fed_id,
        peer_num: num_peers.total(),
        cfg: typed_cfg.clone(),
        db: module_db.clone(),
        module_root_secret: root_secret.derive_module_secret(kind),
        api: api.clone(),
        module_api: api.with_scope(api_scope),
        context: picomint_client_module::module::ClientContext::new(
            final_client,
            kind,
            api.clone(),
            api_scope,
            db.clone(),
            module_db,
            full_config.clone(),
            fed_id,
            log_event_added_tx.clone(),
        ),
        task_group: task_group.clone(),
        connector_registry: connectors.clone(),
    };

    Ok(Arc::new(<I as ClientModuleInit>::init(init, &args).await?))
}

#[allow(clippy::too_many_arguments)]
async fn schedule_recovery<I: ClientModuleInit>(
    init: &I,
    kind: ModuleKind,
    api_scope: ApiScope,
    cfg: <<I as picomint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig,
    full_config: &ConsensusConfig,
    db: &Database,
    api: &FederationApi,
    task_group: &TaskGroup,
    root_secret: &DerivableSecret,
    final_client: FinalClientIface,
    fed_id: FederationId,
    num_peers: NumPeers,
    log_event_added_tx: &watch::Sender<()>,
    module_recoveries: &mut BTreeMap<
        ModuleKind,
        Pin<Box<dyn Future<Output = anyhow::Result<()>> + Send>>,
    >,
    module_recovery_progress_receivers: &mut BTreeMap<
        ModuleKind,
        watch::Receiver<RecoveryProgress>,
    >,
) -> anyhow::Result<()>
where
    I: Clone,
{
    let existing_recovery_state = db
        .begin_read()
        .await
        .as_ref()
        .get(&CLIENT_MODULE_RECOVERY, &ClientModuleRecovery { kind });

    let progress = match existing_recovery_state {
        Some(state) if state.is_done() => {
            debug!(kind = %kind, "Module recovery already complete");
            return Ok(());
        }
        Some(state) => state.progress,
        None => {
            let progress = RecoveryProgress::none();
            let dbtx = db.begin_write().await;
            let tx = dbtx.as_ref();
            picomint_eventlog::log_event(
                &tx,
                log_event_added_tx.clone(),
                None,
                ModuleRecoveryStarted::new(kind),
            );
            tx.insert(
                &CLIENT_MODULE_RECOVERY,
                &ClientModuleRecovery { kind },
                &ClientModuleRecoveryState { progress },
            );
            dbtx.commit().await;
            progress
        }
    };

    let (progress_tx, progress_rx) = tokio::sync::watch::channel(progress);
    let module_db = db.isolate(format!("module-{kind}"));
    let module_api = api.with_scope(api_scope);
    let init_clone = init.clone();
    let api_clone = api.clone();
    let db_clone = db.clone();
    let task_group_clone = task_group.clone();
    let root_secret_clone = root_secret.clone();
    let final_client_clone = final_client.clone();
    let full_config_clone = full_config.clone();
    let log_event_added_tx_clone = log_event_added_tx.clone();

    let recover_fut = Box::pin(async move {
        let args = picomint_client_module::module::init::ClientModuleRecoverArgs {
            federation_id: fed_id,
            num_peers,
            cfg: cfg.clone(),
            db: module_db.clone(),
            module_root_secret: root_secret_clone.derive_module_secret(kind),
            api: api_clone.clone(),
            module_api,
            context: picomint_client_module::module::ClientContext::new(
                final_client_clone,
                kind,
                api_clone,
                api_scope,
                db_clone,
                module_db,
                full_config_clone,
                fed_id,
                log_event_added_tx_clone,
            ),
            progress_tx,
            task_group: task_group_clone,
        };
        <I as ClientModuleInit>::recover(&init_clone, &args)
            .await
            .inspect_err(|err| {
                warn!(
                    target: LOG_CLIENT,
                    kind = %kind,
                    err = %format_args!("{err:#}"), "Module failed to recover"
                );
            })
    });

    module_recoveries.insert(kind, recover_fut);
    module_recovery_progress_receivers.insert(kind, progress_rx);
    Ok(())
}

/// An intermediate step before Client joining or recovering
///
/// Meant to support showing user some initial information about the Federation
/// before actually joining.
pub struct ClientPreview {
    inner: ClientBuilder,
    config: ConsensusConfig,
    connectors: Endpoint,
}

impl ClientPreview {
    /// Get the config
    pub fn config(&self) -> &ConsensusConfig {
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
