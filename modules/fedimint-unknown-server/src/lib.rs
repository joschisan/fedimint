#![deny(clippy::pedantic)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

use anyhow::bail;
use async_trait::async_trait;
use fedimint_core::config::{
    ServerModuleConfig, ServerModuleConsensusConfig, TypedServerModuleConfig,
    TypedServerModuleConsensusConfig,
};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    ApiEndpoint, CoreConsensusVersion, InputMeta, ModuleConsensusVersion, ModuleInit,
    TransactionItemAmounts,
};
use fedimint_core::{InPoint, OutPoint, PeerId};
use fedimint_redb::v2::{ReadTxRef, WriteTxRef};
use fedimint_server_core::config::PeerHandleOps;
use fedimint_server_core::{
    ConfigGenModuleArgs, ServerModule, ServerModuleInit, ServerModuleInitArgs,
};
pub use fedimint_unknown_common as common;
use fedimint_unknown_common::config::{
    UnknownClientConfig, UnknownConfig, UnknownConfigConsensus, UnknownConfigPrivate,
};
use fedimint_unknown_common::{
    MODULE_CONSENSUS_VERSION, UnknownCommonInit, UnknownConsensusItem, UnknownInput,
    UnknownInputError, UnknownModuleTypes, UnknownOutput, UnknownOutputError,
};
pub mod db;

/// Generates the module
#[derive(Debug, Clone)]
pub struct UnknownInit;

// TODO: Boilerplate-code
impl ModuleInit for UnknownInit {
    type Common = UnknownCommonInit;
}

/// Implementation of server module non-consensus functions
#[async_trait]
impl ServerModuleInit for UnknownInit {
    type Module = Unknown;

    /// Returns the version of this module
    fn versions(&self, _core: CoreConsensusVersion) -> &[ModuleConsensusVersion] {
        &[MODULE_CONSENSUS_VERSION]
    }

    /// Initialize the module
    async fn init(&self, args: &ServerModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        Ok(Unknown::new(args.cfg().to_typed()?))
    }

    async fn distributed_gen(
        &self,
        _peers: &(dyn PeerHandleOps + Send + Sync),
        _args: &ConfigGenModuleArgs,
    ) -> anyhow::Result<ServerModuleConfig> {
        Ok(UnknownConfig {
            private: UnknownConfigPrivate,
            consensus: UnknownConfigConsensus {},
        }
        .to_erased())
    }

    /// Converts the consensus config into the client config
    fn get_client_config(
        &self,
        config: &ServerModuleConsensusConfig,
    ) -> anyhow::Result<UnknownClientConfig> {
        let _config = UnknownConfigConsensus::from_erased(config)?;
        Ok(UnknownClientConfig {})
    }

    fn validate_config(
        &self,
        _identity: &PeerId,
        _config: ServerModuleConfig,
    ) -> anyhow::Result<()> {
        Ok(())
    }

}

/// Unknown module
#[derive(Debug)]
pub struct Unknown {
    pub cfg: UnknownConfig,
}

/// Implementation of consensus for the server module
#[async_trait]
impl ServerModule for Unknown {
    /// Define the consensus types
    type Common = UnknownModuleTypes;
    type Init = UnknownInit;

    async fn consensus_proposal(&self, _dbtx: &ReadTxRef<'_>) -> Vec<UnknownConsensusItem> {
        Vec::new()
    }

    async fn process_consensus_item(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _consensus_item: UnknownConsensusItem,
        _peer_id: PeerId,
    ) -> anyhow::Result<()> {
        // WARNING: `process_consensus_item` should return an `Err` for items that do
        // not change any internal consensus state. Failure to do so, will result in an
        // (potentially significantly) increased consensus history size.
        // If you are using this code as a template,
        // make sure to read the [`ServerModule::process_consensus_item`] documentation,
        bail!("The unknown module does not use consensus items");
    }

    async fn process_input(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _input: &UnknownInput,
        _in_point: InPoint,
    ) -> Result<InputMeta, UnknownInputError> {
        unreachable!();
    }

    async fn process_output(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _output: &UnknownOutput,
        _out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, UnknownOutputError> {
        unreachable!();
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

impl Unknown {
    /// Create new module instance
    pub fn new(cfg: UnknownConfig) -> Unknown {
        Unknown { cfg }
    }
}
