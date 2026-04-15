// TODO: remove and fix nits
#![allow(clippy::pedantic)]

use std::collections::BTreeSet;
use std::marker::PhantomData;
use std::sync::Arc;
use std::{any, marker};

use bitcoin::Network;
pub use fedimint_api_client::api::DynGlobalApi;
use fedimint_api_client::api::DynModuleApi;
use fedimint_core::config::{
    ClientModuleConfig, CommonModuleInitRegistry, ModuleInitRegistry, ServerModuleConfig,
    ServerModuleConsensusConfig,
};
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::module::{
    CommonModuleInit, CoreConsensusVersion, IDynCommonModuleInit, ModuleConsensusVersion,
    ModuleInit,
};
use fedimint_core::task::TaskGroup;
use fedimint_core::{NumPeers, PeerId, apply, async_trait_maybe_send, dyn_newtype_define};
use fedimint_redb::v2::Database as V2Database;

use crate::bitcoin_rpc::ServerBitcoinRpcMonitor;
use crate::config::PeerHandleOps;
use crate::{DynServerModule, ServerModule};

/// Arguments passed to modules during config generation
///
/// This replaces the per-module GenParams approach with a unified struct
/// containing all the information modules need for DKG/config generation.
#[derive(Debug, Clone, Copy)]
pub struct ConfigGenModuleArgs {
    /// Bitcoin network for the federation
    pub network: Network,
}

/// Interface for Module Generation
///
/// This trait contains the methods responsible for the module's
/// - initialization
/// - config generation
/// - config validation
///
/// Once the module configuration is ready, the module can be instantiated via
/// `[Self::init]`.
#[apply(async_trait_maybe_send!)]
pub trait IServerModuleInit: IDynCommonModuleInit {
    fn as_common(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static);

    /// Initialize the [`DynServerModule`] instance from its config
    #[allow(clippy::too_many_arguments)]
    async fn init(
        &self,
        peer_num: NumPeers,
        cfg: ServerModuleConfig,
        db: V2Database,
        task_group: &TaskGroup,
        our_peer_id: PeerId,
        module_api: DynModuleApi,
        server_bitcoin_rpc_monitor: ServerBitcoinRpcMonitor,
    ) -> anyhow::Result<DynServerModule>;

    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig>;

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()>;

    fn get_client_config(
        &self,
        module_instance_id: ModuleInstanceId,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<ClientModuleConfig>;
}

/// A type that can be used as module-shared value inside
/// [`ServerModuleInitArgs`]
pub trait ServerModuleShared: any::Any + Send + Sync {
    fn new(task_group: TaskGroup) -> Self;
}

pub struct ServerModuleInitArgs<S>
where
    S: ServerModuleInit,
{
    cfg: ServerModuleConfig,
    db: V2Database,
    task_group: TaskGroup,
    our_peer_id: PeerId,
    num_peers: NumPeers,
    module_api: DynModuleApi,
    server_bitcoin_rpc_monitor: ServerBitcoinRpcMonitor,
    // ClientModuleInitArgs needs a bound because sometimes we need
    // to pass associated-types data, so let's just put it here right away
    _marker: marker::PhantomData<S>,
}

impl<S> ServerModuleInitArgs<S>
where
    S: ServerModuleInit,
{
    pub fn cfg(&self) -> &ServerModuleConfig {
        &self.cfg
    }

    pub fn db(&self) -> &V2Database {
        &self.db
    }

    pub fn num_peers(&self) -> NumPeers {
        self.num_peers
    }

    pub fn task_group(&self) -> &TaskGroup {
        &self.task_group
    }

    pub fn our_peer_id(&self) -> PeerId {
        self.our_peer_id
    }

    pub fn module_api(&self) -> &DynModuleApi {
        &self.module_api
    }

    pub fn server_bitcoin_rpc_monitor(&self) -> ServerBitcoinRpcMonitor {
        self.server_bitcoin_rpc_monitor.clone()
    }
}
/// Module Generation trait with associated types
///
/// Needs to be implemented by module generation type
///
/// For examples, take a look at one of the `MintConfigGenerator`,
/// `WalletConfigGenerator`, or `LightningConfigGenerator` structs.
#[apply(async_trait_maybe_send!)]
pub trait ServerModuleInit: ModuleInit + Sized {
    type Module: ServerModule + Send + Sync;

    /// Version of the module consensus supported by this implementation given a
    /// certain [`CoreConsensusVersion`].
    ///
    /// Refer to [`ModuleConsensusVersion`] for more information about
    /// versioning.
    ///
    /// One module implementation ([`ServerModuleInit`] of a given
    /// [`ModuleKind`]) can potentially implement multiple versions of the
    /// consensus, and depending on the config module instance config,
    /// instantiate the desired one. This method should expose all the
    /// available versions, purely for information, setup UI and sanity
    /// checking purposes.
    fn versions(&self, core: CoreConsensusVersion) -> &[ModuleConsensusVersion];

    fn kind() -> ModuleKind {
        <Self as ModuleInit>::Common::KIND
    }

    /// Initialize the module instance from its config
    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module>;

    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig>;

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()>;

    /// Converts the consensus config into the client config
    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<<<Self as ModuleInit>::Common as CommonModuleInit>::ClientConfig>;
}

#[apply(async_trait_maybe_send!)]
impl<T> IServerModuleInit for T
where
    T: ServerModuleInit + 'static + Sync,
{
    fn as_common(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static) {
        self
    }

    async fn init(
        &self,
        num_peers: NumPeers,
        cfg: ServerModuleConfig,
        db: V2Database,
        task_group: &TaskGroup,
        our_peer_id: PeerId,
        module_api: DynModuleApi,
        server_bitcoin_rpc_monitor: ServerBitcoinRpcMonitor,
    ) -> anyhow::Result<DynServerModule> {
        let module = <Self as ServerModuleInit>::init(
            self,
            &ServerModuleInitArgs {
                num_peers,
                cfg,
                db,
                task_group: task_group.clone(),
                our_peer_id,
                _marker: PhantomData,
                module_api,
                server_bitcoin_rpc_monitor,
            },
        )
        .await?;

        Ok(DynServerModule::from(module))
    }

    async fn distributed_gen(
        &self,
        peers: &(dyn PeerHandleOps + Send + Sync),
        args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        <Self as ServerModuleInit>::distributed_gen(self, peers, args).await
    }

    fn validate_config(&self, identity: &PeerId, config: ServerModuleConfig) -> anyhow::Result<()> {
        <Self as ServerModuleInit>::validate_config(self, identity, config)
    }

    fn get_client_config(
        &self,
        module_instance_id: ModuleInstanceId,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<ClientModuleConfig> {
        ClientModuleConfig::from_typed(
            module_instance_id,
            <Self as ServerModuleInit>::kind(),
            config.version,
            <Self as ServerModuleInit>::get_client_config(self, config)?,
        )
    }
}

dyn_newtype_define!(
    #[derive(Clone)]
    pub DynServerModuleInit(Arc<IServerModuleInit>)
);

impl AsRef<dyn IDynCommonModuleInit + Send + Sync + 'static> for DynServerModuleInit {
    fn as_ref(&self) -> &(dyn IDynCommonModuleInit + Send + Sync + 'static) {
        self.inner.as_common()
    }
}

pub type ServerModuleInitRegistry = ModuleInitRegistry<DynServerModuleInit>;

pub trait ServerModuleInitRegistryExt {
    fn to_common(&self) -> CommonModuleInitRegistry;
    fn default_modules(&self) -> BTreeSet<ModuleKind>;
}

impl ServerModuleInitRegistryExt for ServerModuleInitRegistry {
    fn to_common(&self) -> CommonModuleInitRegistry {
        self.iter().map(|(_k, v)| v.to_dyn_common()).collect()
    }

    fn default_modules(&self) -> BTreeSet<ModuleKind> {
        self.iter().map(|(kind, _init)| kind.clone()).collect()
    }
}
