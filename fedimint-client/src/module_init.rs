use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use fedimint_api_client::api::DynGlobalApi;
use fedimint_api_client::connection::ConnectionPool;
use fedimint_client_module::db::ClientModuleMigrationFn;
use fedimint_client_module::module::init::{
    ClientModuleInit, ClientModuleInitArgs, ClientModuleRecoverArgs,
};
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::module::{ClientContext, DynClientModule, FinalClientIface};
use fedimint_client_module::{ClientModule, ModuleInstanceId, ModuleKind};
use fedimint_core::config::{ClientModuleConfig, FederationId, ModuleInitRegistry};
use fedimint_core::core::Decoder;
use fedimint_core::db::{Database, DatabaseVersion};
use fedimint_core::module::{CommonModuleInit, IDynCommonModuleInit, ModuleInit};
use fedimint_core::task::{MaybeSend, MaybeSync, TaskGroup};
use fedimint_core::{NumPeers, apply, async_trait_maybe_send, dyn_newtype_define};
use fedimint_derive_secret::DerivableSecret;
use tokio::sync::watch;

use crate::sm::notifier::Notifier;

pub type ClientModuleInitRegistry = ModuleInitRegistry<DynClientModuleInit>;

#[apply(async_trait_maybe_send!)]
pub trait IClientModuleInit: IDynCommonModuleInit + fmt::Debug + MaybeSend + MaybeSync {
    fn decoder(&self) -> Decoder;

    fn module_kind(&self) -> ModuleKind;

    fn as_common(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static);

    #[allow(clippy::too_many_arguments)]
    async fn recover(
        &self,
        final_client: FinalClientIface,
        federation_id: FederationId,
        num_peers: NumPeers,
        cfg: ClientModuleConfig,
        db: Database,
        instance_id: ModuleInstanceId,
        module_root_secret: DerivableSecret,
        notifier: Notifier,
        api: DynGlobalApi,
        progress_tx: watch::Sender<RecoveryProgress>,
        task_group: TaskGroup,
    ) -> anyhow::Result<()>;

    #[allow(clippy::too_many_arguments)]
    async fn init(
        &self,
        final_client: FinalClientIface,
        federation_id: FederationId,
        peer_num: usize,
        cfg: ClientModuleConfig,
        db: Database,
        instance_id: ModuleInstanceId,
        module_root_secret: DerivableSecret,
        notifier: Notifier,
        api: DynGlobalApi,
        task_group: TaskGroup,
        connector_registry: ConnectionPool,
    ) -> anyhow::Result<DynClientModule>;

    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ClientModuleMigrationFn>;

    /// See [`ClientModuleInit::used_db_prefixes`]
    fn used_db_prefixes(&self) -> Option<BTreeSet<u8>>;
}

#[apply(async_trait_maybe_send!)]
impl<T> IClientModuleInit for T
where
    T: ClientModuleInit + 'static + MaybeSend + Sync,
{
    fn decoder(&self) -> Decoder {
        <<T as ClientModuleInit>::Module as ClientModule>::decoder()
    }

    fn module_kind(&self) -> ModuleKind {
        <Self as ModuleInit>::Common::KIND
    }

    fn as_common(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static) {
        self
    }

    async fn recover(
        &self,
        final_client: FinalClientIface,
        federation_id: FederationId,
        num_peers: NumPeers,
        cfg: ClientModuleConfig,
        db: Database,
        instance_id: ModuleInstanceId,
        module_root_secret: DerivableSecret,
        // TODO: make dyn type for notifier
        notifier: Notifier,
        api: DynGlobalApi,
        progress_tx: watch::Sender<RecoveryProgress>,
        task_group: TaskGroup,
    ) -> anyhow::Result<()> {
        let typed_cfg: &<<T as fedimint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig = cfg.cast()?;

        let (module_db, global_dbtx_access_token) = db.with_prefix_module_id(instance_id);
        Ok(<Self as ClientModuleInit>::recover(
            self,
            &ClientModuleRecoverArgs {
                federation_id,
                num_peers,
                cfg: typed_cfg.clone(),
                db: module_db.clone(),
                module_root_secret,
                notifier: notifier.module_notifier(instance_id, final_client.clone()),
                api: api.clone(),
                module_api: api.with_module(instance_id),
                context: ClientContext::new(
                    final_client,
                    instance_id,
                    global_dbtx_access_token,
                    module_db,
                ),
                progress_tx,
                task_group,
            },
        )
        .await?)
    }

    async fn init(
        &self,
        final_client: FinalClientIface,
        federation_id: FederationId,
        peer_num: usize,
        cfg: ClientModuleConfig,
        db: Database,
        instance_id: ModuleInstanceId,
        module_root_secret: DerivableSecret,
        // TODO: make dyn type for notifier
        notifier: Notifier,
        api: DynGlobalApi,
        task_group: TaskGroup,
        connector_registry: ConnectionPool,
    ) -> anyhow::Result<DynClientModule> {
        let typed_cfg: &<<T as fedimint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig = cfg.cast()?;
        let (module_db, global_dbtx_access_token) = db.with_prefix_module_id(instance_id);
        Ok(<Self as ClientModuleInit>::init(
            self,
            &ClientModuleInitArgs {
                federation_id,
                peer_num,
                cfg: typed_cfg.clone(),
                db: module_db.clone(),
                module_root_secret,
                notifier: notifier.module_notifier(instance_id, final_client.clone()),
                api: api.clone(),
                module_api: api.with_module(instance_id),
                context: ClientContext::new(
                    final_client,
                    instance_id,
                    global_dbtx_access_token,
                    module_db,
                ),
                task_group,
                connector_registry,
            },
        )
        .await?
        .into())
    }

    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ClientModuleMigrationFn> {
        <Self as ClientModuleInit>::get_database_migrations(self)
    }

    fn used_db_prefixes(&self) -> Option<BTreeSet<u8>> {
        <Self as ClientModuleInit>::used_db_prefixes(self)
    }
}

dyn_newtype_define!(
    #[derive(Clone)]
    pub DynClientModuleInit(Arc<IClientModuleInit>)
);

impl AsRef<dyn IDynCommonModuleInit + Send + Sync + 'static> for DynClientModuleInit {
    fn as_ref(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static) {
        self.inner.as_common()
    }
}

impl AsRef<dyn IClientModuleInit + 'static> for DynClientModuleInit {
    fn as_ref(&self) -> &(dyn IClientModuleInit + 'static) {
        self.inner.as_ref()
    }
}
