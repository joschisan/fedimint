use fedimint_api_client::Endpoint;
use fedimint_api_client::api::{DynGlobalApi, DynModuleApi};
use fedimint_core::config::FederationId;
use fedimint_core::core::ModuleKind;
use fedimint_core::module::{CommonModuleInit, ModuleInit};
use fedimint_core::task::TaskGroup;
use fedimint_core::{NumPeers, apply, async_trait_maybe_send};
use fedimint_derive_secret::DerivableSecret;
use fedimint_logging::LOG_CLIENT;
use fedimint_redb::v2::Database;
use tracing::warn;

use super::ClientContext;
use super::recovery::RecoveryProgress;
use crate::module::ClientModule;

pub struct ClientModuleInitArgs<C>
where
    C: ClientModuleInit,
{
    pub federation_id: FederationId,
    pub peer_num: usize,
    pub cfg: <<C as ModuleInit>::Common as CommonModuleInit>::ClientConfig,
    pub db: Database,
    pub module_root_secret: DerivableSecret,
    pub api: DynGlobalApi,
    pub module_api: DynModuleApi,
    pub context: ClientContext<<C as ClientModuleInit>::Module>,
    pub task_group: TaskGroup,
    pub connector_registry: Endpoint,
}

impl<C> ClientModuleInitArgs<C>
where
    C: ClientModuleInit,
{
    pub fn federation_id(&self) -> &FederationId {
        &self.federation_id
    }

    pub fn peer_num(&self) -> usize {
        self.peer_num
    }

    pub fn cfg(&self) -> &<<C as ModuleInit>::Common as CommonModuleInit>::ClientConfig {
        &self.cfg
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn module_root_secret(&self) -> &DerivableSecret {
        &self.module_root_secret
    }

    pub fn api(&self) -> &DynGlobalApi {
        &self.api
    }

    pub fn module_api(&self) -> &DynModuleApi {
        &self.module_api
    }

    /// Get the [`ClientContext`] for later use
    ///
    /// Notably `ClientContext` can not be used during `ClientModuleInit::init`,
    /// as the outer context is not yet complete. But it can be stored to be
    /// used in the methods of [`ClientModule`], at which point it will be
    /// ready.
    pub fn context(&self) -> ClientContext<<C as ClientModuleInit>::Module> {
        self.context.clone()
    }

    pub fn task_group(&self) -> &TaskGroup {
        &self.task_group
    }

    pub fn connector_registry(&self) -> &Endpoint {
        &self.connector_registry
    }
}

pub struct ClientModuleRecoverArgs<C>
where
    C: ClientModuleInit,
{
    pub federation_id: FederationId,
    pub num_peers: NumPeers,
    pub cfg: <<C as ModuleInit>::Common as CommonModuleInit>::ClientConfig,
    pub db: Database,
    pub module_root_secret: DerivableSecret,
    pub api: DynGlobalApi,
    pub module_api: DynModuleApi,
    pub context: ClientContext<<C as ClientModuleInit>::Module>,
    pub progress_tx: tokio::sync::watch::Sender<RecoveryProgress>,
    pub task_group: TaskGroup,
}

impl<C> ClientModuleRecoverArgs<C>
where
    C: ClientModuleInit,
{
    pub fn federation_id(&self) -> &FederationId {
        &self.federation_id
    }

    pub fn num_peers(&self) -> NumPeers {
        self.num_peers
    }

    pub fn cfg(&self) -> &<<C as ModuleInit>::Common as CommonModuleInit>::ClientConfig {
        &self.cfg
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn task_group(&self) -> &TaskGroup {
        &self.task_group
    }

    pub fn module_root_secret(&self) -> &DerivableSecret {
        &self.module_root_secret
    }

    pub fn api(&self) -> &DynGlobalApi {
        &self.api
    }

    pub fn module_api(&self) -> &DynModuleApi {
        &self.module_api
    }

    /// Get the [`ClientContext`]
    ///
    /// Notably `ClientContext`, unlike [`ClientModuleInitArgs::context`],
    /// the client context is guaranteed to be usable immediately.
    pub fn context(&self) -> ClientContext<<C as ClientModuleInit>::Module> {
        self.context.clone()
    }

    pub fn update_recovery_progress(&self, progress: RecoveryProgress) {
        // we want a warning if the send channel was not connected to
        #[allow(clippy::disallowed_methods)]
        if progress.is_done() {
            // Recovery is complete when the recovery function finishes. To avoid
            // confusing any downstream code, we never send completed process.
            warn!(target: LOG_CLIENT, "Module trying to send a completed recovery progress. Ignoring");
        } else if progress.is_none() {
            // Recovery starts with "none" none progress. To avoid
            // confusing any downstream code, we never send none process afterwards.
            warn!(target: LOG_CLIENT, "Module trying to send a none recovery progress. Ignoring");
        } else if self.progress_tx.send(progress).is_err() {
            warn!(target: LOG_CLIENT, "Module trying to send a recovery progress but nothing is listening");
        }
    }
}

#[apply(async_trait_maybe_send!)]
pub trait ClientModuleInit: ModuleInit + Sized {
    type Module: ClientModule;

    fn kind() -> ModuleKind {
        <Self::Module as ClientModule>::kind()
    }

    /// Recover the state of the client module.
    ///
    /// If `Err` is returned, the higher level client/application might try
    /// again at a different time (client restarted, code version changed, etc.)
    async fn recover(&self, _args: &ClientModuleRecoverArgs<Self>) -> anyhow::Result<()> {
        warn!(
            target: LOG_CLIENT,
            kind = %<Self::Module as ClientModule>::kind(),
            "Module does not support recovery, completing without doing anything"
        );
        Ok(())
    }

    /// Initialize a [`ClientModule`] instance from its config
    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module>;
}
