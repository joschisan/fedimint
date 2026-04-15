#![deny(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use std::collections::BTreeMap;

use anyhow::bail;
use async_trait::async_trait;
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::DatabaseVersion;
use fedimint_core::db::v2::WriteTxRef;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    ApiEndpoint, CoreConsensusVersion, InputMeta, ModuleConsensusVersion, ModuleInit,
    TransactionItemAmounts,
};
use fedimint_core::{InPoint, OutPoint, PeerId};
pub use fedimint_empty_common as common;
use fedimint_empty_common::config::{
    EmptyClientConfig, EmptyConfig, EmptyConfigConsensus, EmptyConfigPrivate,
};
use fedimint_empty_common::{
    EmptyCommonInit, EmptyConsensusItem, EmptyInput, EmptyInputError, EmptyModuleTypes,
    EmptyOutput, EmptyOutputError, MODULE_CONSENSUS_VERSION,
};
use fedimint_server_core::config::PeerHandleOps;
use fedimint_server_core::migration::ServerModuleDbMigrationFn;
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
pub mod db;

/// Generates the module
#[derive(Debug, Clone)]
pub struct EmptyInit;

// TODO: Boilerplate-code
impl ModuleInit for EmptyInit {
    type Common = EmptyCommonInit;

}

/// Implementation of server module non-consensus functions
#[async_trait]
impl ServerModuleInit for EmptyInit {
    type Module = Empty;

    /// Returns the version of this module
    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    /// Initialize the module
    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(Empty::new(args.cfg().to_typed()?))
    }

    async fn distributed_gen(
        &self,
        _peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        Ok(EmptyConfig {
            private: EmptyConfigPrivate,
            consensus: EmptyConfigConsensus {},
        }
        .to_erased())
    }

    /// Converts the consensus config into the client config
    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<EmptyClientConfig> {
        let _config = EmptyConfigConsensus::from_erased(config)?;
        Ok(EmptyClientConfig {})
    }

    fn validate_config(
        &self,
        _identity: &PeerId,
        _config: ServerModuleConfig,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    /// DB migrations to move from old to newer versions
    fn get_database_migrations(
        &self,
    ) -> BTreeMap<DatabaseVersion, ServerModuleDbMigrationFn<Empty>> {
        BTreeMap::new()
    }
}

/// Empty module
#[derive(Debug)]
pub struct Empty {
    pub cfg: EmptyConfig,
}

/// Implementation of consensus for the server module
#[async_trait]
impl ServerModule for Empty {
    /// Define the consensus types
    type Common = EmptyModuleTypes;
    type Init = EmptyInit;

    async fn consensus_proposal(&self, _dbtx: &WriteTxRef<'_>) -> Vec<EmptyConsensusItem> {
        Vec::new()
    }

    async fn process_consensus_item(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _consensus_item: EmptyConsensusItem,
        _peer_id: PeerId,
    ) -> anyhow::Result<()> {
        bail!("The empty module does not use consensus items");
    }

    async fn process_input(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _input: &EmptyInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, EmptyInputError> {
        Err(EmptyInputError::NotSupported)
    }

    async fn process_output(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _output: &EmptyOutput,
        _out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, EmptyOutputError> {
        Err(EmptyOutputError::NotSupported)
    }

    async fn audit(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _audit: &mut Audit,
        _module_instance_id: ModuleInstanceId,
    ) {
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>> {
        Vec::new()
    }
}

impl Empty {
    /// Create new module instance
    pub fn new(cfg: EmptyConfig) -> Empty {
        Empty { cfg }
    }
}
