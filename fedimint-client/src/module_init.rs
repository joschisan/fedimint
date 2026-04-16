use std::fmt;
use std::sync::Arc;

use fedimint_api_client::Endpoint;
use fedimint_api_client::api::DynGlobalApi;
use fedimint_client_module::module::init::{
    ClientModuleInit, ClientModuleInitArgs, ClientModuleRecoverArgs,
};
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::module::{ClientContext, DynClientModule, FinalClientIface};
use fedimint_client_module::{ClientModule, ModuleInstanceId, ModuleKind};
use fedimint_core::config::{ClientModuleConfig, FederationId, ModuleInitRegistry};
use fedimint_core::core::Decoder;
use fedimint_core::module::{CommonModuleInit, IDynCommonModuleInit, ModuleInit};
use fedimint_core::task::{MaybeSend, MaybeSync, TaskGroup};
use fedimint_core::{NumPeers, apply, async_trait_maybe_send, dyn_newtype_define};
use fedimint_derive_secret::DerivableSecret;
use fedimint_redb::Database;
use tokio::sync::watch;

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
        api: DynGlobalApi,
        task_group: TaskGroup,
        connector_registry: Endpoint,
    ) -> anyhow::Result<DynClientModule>;
}

#[apply(async_trait_maybe_send!)]
impl<T> IClientModuleInit for T
where
    T: ClientModuleInit + 'static + MaybeSend + Sync,
    fedimint_api_client::wire::Input: From<
        <<<T as ClientModuleInit>::Module as ClientModule>::Common as fedimint_core::module::ModuleCommon>::Input,
    >,
    fedimint_api_client::wire::Output: From<
        <<<T as ClientModuleInit>::Module as ClientModule>::Common as fedimint_core::module::ModuleCommon>::Output,
    >,
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
        api: DynGlobalApi,
        progress_tx: watch::Sender<RecoveryProgress>,
        task_group: TaskGroup,
    ) -> anyhow::Result<()> {
        let typed_cfg: &<<T as fedimint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig = cfg.cast()?;

        let module_db = db.isolate(format!("module-{instance_id}"));
        Ok(<Self as ClientModuleInit>::recover(
            self,
            &ClientModuleRecoverArgs {
                federation_id,
                num_peers,
                cfg: typed_cfg.clone(),
                db: module_db.clone(),
                module_root_secret,
                api: api.clone(),
                module_api: api.with_module(instance_id),
                context: ClientContext::new(final_client, instance_id, module_db),
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
        api: DynGlobalApi,
        task_group: TaskGroup,
        connector_registry: Endpoint,
    ) -> anyhow::Result<DynClientModule> {
        let typed_cfg: &<<T as fedimint_core::module::ModuleInit>::Common as CommonModuleInit>::ClientConfig = cfg.cast()?;
        let module_db = db.isolate(format!("module-{instance_id}"));
        Ok(<Self as ClientModuleInit>::init(
            self,
            &ClientModuleInitArgs {
                federation_id,
                peer_num,
                cfg: typed_cfg.clone(),
                db: module_db.clone(),
                module_root_secret,
                api: api.clone(),
                module_api: api.with_module(instance_id),
                context: ClientContext::new(final_client, instance_id, module_db),
                task_group,
                connector_registry,
            },
        )
        .await?
        .into())
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
