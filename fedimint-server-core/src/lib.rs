//! Fedimint Core Server module interface
//!
//! Fedimint supports externally implemented modules.
//!
//! This (Rust) module defines common interoperability types
//! and functionality that are only used on the server side.

pub mod bitcoin_rpc;
pub mod config;
pub mod dashboard_ui;
mod init;
pub mod setup_ui;

use std::any::Any;
use std::fmt::Debug;
use std::sync::Arc;

use fedimint_core::core::{
    Decoder, DynInput, DynInputError, DynModuleConsensusItem, DynOutput, DynOutputError,
    ModuleInstanceId, ModuleKind,
};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::module::{
    ApiEndpoint, ApiRequestErased, CommonModuleInit, InputMeta, ModuleCommon, ModuleInit,
    TransactionItemAmounts,
};
use fedimint_core::{InPoint, OutPoint, PeerId, apply, async_trait_maybe_send, dyn_newtype_define};
use fedimint_redb::v2::{ReadTxRef, WriteTxRef};
pub use init::*;

#[apply(async_trait_maybe_send!)]
pub trait ServerModule: Debug + Sized {
    type Common: ModuleCommon;

    type Init: ServerModuleInit;

    fn module_kind() -> ModuleKind {
        // Note: All modules should define kinds as &'static str, so this doesn't
        // allocate
        <Self::Init as ModuleInit>::Common::KIND
    }

    /// Returns a decoder for the following associated types of this module:
    /// * `ClientConfig`
    /// * `Input`
    /// * `Output`
    /// * `OutputOutcome`
    /// * `ConsensusItem`
    /// * `InputError`
    /// * `OutputError`
    fn decoder() -> Decoder {
        Self::Common::decoder_builder().build()
    }

    /// This module's contribution to the next consensus proposal. This method
    /// is only guaranteed to be called once every few seconds. Consensus items
    /// are not meant to be latency critical; do not create them as
    /// a response to a processed transaction. Only use consensus items to
    /// establish consensus on a value that is required to verify
    /// transactions, like unix time, block heights and feerates, and model all
    /// other state changes trough transactions. The intention for this method
    /// is to always return all available consensus items even if they are
    /// redundant while process_consensus_item returns an error for the
    /// redundant proposals.
    ///
    /// If you think you actually do require latency critical consensus items or
    /// have trouble designing your module in order to avoid them please contact
    /// the Fedimint developers.
    async fn consensus_proposal(
        &self,
        dbtx: &ReadTxRef<'_>,
    ) -> Vec<<Self::Common as ModuleCommon>::ConsensusItem>;

    /// This function is called once for every consensus item. The function
    /// should return Ok if and only if the consensus item changes
    /// the system state. *Therefore this method should return an error in case
    /// of merely redundant consensus items such that they will be purged from
    /// the history of the federation.* This enables consensus_proposal to
    /// return all available consensus item without wasting disk
    /// space with redundant consensus items.
    async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: <Self::Common as ModuleCommon>::ConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()>;

    /// Try to spend a transaction input. On success all necessary updates will
    /// be part of the database transaction. On failure (e.g. double spend)
    /// the database transaction is rolled back and the operation will take
    /// no effect.
    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &<Self::Common as ModuleCommon>::Input,
        in_point: InPoint,
    ) -> Result<InputMeta, <Self::Common as ModuleCommon>::InputError>;

    /// Try to create an output (e.g. issue notes, peg-out BTC, …). On success
    /// all necessary updates to the database will be part of the database
    /// transaction. On failure (e.g. double spend) the database transaction
    /// is rolled back and the operation will take no effect.
    ///
    /// The supplied `out_point` identifies the operation (e.g. a peg-out or
    /// note issuance) and can be used to retrieve its outcome later using
    /// module-specific API endpoints.
    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &<Self::Common as ModuleCommon>::Output,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, <Self::Common as ModuleCommon>::OutputError>;

    /// Queries the database and returns all assets and liabilities of the
    /// module.
    ///
    /// Summing over all modules, if liabilities > assets then an error has
    /// occurred in the database and consensus should halt.
    async fn audit(
        &self,
        dbtx: &WriteTxRef<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    );

    /// Returns a list of custom API endpoints defined by the module. These are
    /// made available both to users as well as to other modules. They thus
    /// should be deterministic, only dependant on their input and the
    /// current epoch.
    fn api_endpoints(&self) -> Vec<ApiEndpoint<Self>>;
}

/// Backend side module interface
///
/// Server side Fedimint module needs to implement this trait.
#[apply(async_trait_maybe_send!)]
pub trait IServerModule: Debug {
    fn as_any(&self) -> &dyn Any;

    /// Returns the decoder belonging to the server module
    fn decoder(&self) -> Decoder;

    fn module_kind(&self) -> ModuleKind;

    /// This module's contribution to the next consensus proposal
    async fn consensus_proposal(
        &self,
        dbtx: &ReadTxRef<'_>,
        module_instance_id: ModuleInstanceId,
    ) -> Vec<DynModuleConsensusItem>;

    /// This function is called once for every consensus item. The function
    /// returns an error if any only if the consensus item does not change
    /// our state and therefore may be safely discarded by the atomic broadcast.
    async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: &DynModuleConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()>;

    /// Try to spend a transaction input. On success all necessary updates will
    /// be part of the database transaction. On failure (e.g. double spend)
    /// the database transaction is rolled back and the operation will take
    /// no effect.
    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &DynInput,
        in_point: InPoint,
    ) -> Result<InputMeta, DynInputError>;

    /// Try to create an output (e.g. issue notes, peg-out BTC, …). On success
    /// all necessary updates to the database will be part of the database
    /// transaction. On failure (e.g. double spend) the database transaction
    /// is rolled back and the operation will take no effect.
    ///
    /// The supplied `out_point` identifies the operation (e.g. a peg-out or
    /// note issuance) and can be used to retrieve its outcome later using
    /// `output_status`.
    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &DynOutput,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, DynOutputError>;

    /// Queries the database and returns all assets and liabilities of the
    /// module.
    ///
    /// Summing over all modules, if liabilities > assets then an error has
    /// occurred in the database and consensus should halt.
    async fn audit(
        &self,
        dbtx: &WriteTxRef<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    );

    /// Returns a list of custom API endpoints defined by the module. These are
    /// made available both to users as well as to other modules. They thus
    /// should be deterministic, only dependant on their input and the
    /// current epoch.
    fn api_endpoints(&self) -> Vec<ApiEndpoint<DynServerModule>>;
}

dyn_newtype_define!(
    #[derive(Clone)]
    pub DynServerModule(Arc<IServerModule>)
);

#[apply(async_trait_maybe_send!)]
impl<T> IServerModule for T
where
    T: ServerModule + 'static + Sync,
{
    fn decoder(&self) -> Decoder {
        <T::Common as ModuleCommon>::decoder_builder().build()
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn module_kind(&self) -> ModuleKind {
        <Self as ServerModule>::module_kind()
    }

    /// This module's contribution to the next consensus proposal
    async fn consensus_proposal(
        &self,
        dbtx: &ReadTxRef<'_>,
        module_instance_id: ModuleInstanceId,
    ) -> Vec<DynModuleConsensusItem> {
        <Self as ServerModule>::consensus_proposal(self, dbtx)
            .await
            .into_iter()
            .map(|v| DynModuleConsensusItem::from_typed(module_instance_id, v))
            .collect()
    }

    /// This function is called once for every consensus item. The function
    /// returns an error if any only if the consensus item does not change
    /// our state and therefore may be safely discarded by the atomic broadcast.
    async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        consensus_item: &DynModuleConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        <Self as ServerModule>::process_consensus_item(
            self,
            dbtx,
            Clone::clone(
                consensus_item.as_any()
                    .downcast_ref::<<<Self as ServerModule>::Common as ModuleCommon>::ConsensusItem>()
                    .expect("incorrect consensus item type passed to module plugin"),
            ),
            peer_id
        )
        .await
    }

    /// Try to spend a transaction input. On success all necessary updates will
    /// be part of the database transaction. On failure (e.g. double spend)
    /// the database transaction is rolled back and the operation will take
    /// no effect.
    async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &DynInput,
        in_point: InPoint,
    ) -> Result<InputMeta, DynInputError> {
        <Self as ServerModule>::process_input(
            self,
            dbtx,
            input
                .as_any()
                .downcast_ref::<<<Self as ServerModule>::Common as ModuleCommon>::Input>()
                .expect("incorrect input type passed to module plugin"),
            in_point,
        )
        .await
        .map_err(|v| DynInputError::from_typed(input.module_instance_id(), v))
    }

    /// Try to create an output (e.g. issue notes, peg-out BTC, …). On success
    /// all necessary updates to the database will be part of the database
    /// transaction. On failure (e.g. double spend) the database transaction
    /// is rolled back and the operation will take no effect.
    ///
    /// The supplied `out_point` identifies the operation (e.g. a peg-out or
    /// note issuance) and can be used to retrieve its outcome later using
    /// `output_status`.
    async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &DynOutput,
        out_point: OutPoint,
    ) -> Result<TransactionItemAmounts, DynOutputError> {
        <Self as ServerModule>::process_output(
            self,
            dbtx,
            output
                .as_any()
                .downcast_ref::<<<Self as ServerModule>::Common as ModuleCommon>::Output>()
                .expect("incorrect output type passed to module plugin"),
            out_point,
        )
        .await
        .map_err(|v| DynOutputError::from_typed(output.module_instance_id(), v))
    }

    /// Queries the database and returns all assets and liabilities of the
    /// module.
    ///
    /// Summing over all modules, if liabilities > assets then an error has
    /// occurred in the database and consensus should halt.
    async fn audit(
        &self,
        dbtx: &WriteTxRef<'_>,
        audit: &mut Audit,
        module_instance_id: ModuleInstanceId,
    ) {
        <Self as ServerModule>::audit(self, dbtx, audit, module_instance_id).await;
    }

    fn api_endpoints(&self) -> Vec<ApiEndpoint<DynServerModule>> {
        <Self as ServerModule>::api_endpoints(self)
            .into_iter()
            .map(|ApiEndpoint { path, handler }| ApiEndpoint {
                path,
                handler: Box::new(move |module: &DynServerModule, value: ApiRequestErased| {
                    let typed_module = module
                        .as_any()
                        .downcast_ref::<T>()
                        .expect("the dispatcher should always call with the right module");
                    Box::pin(handler(typed_module, value))
                }),
            })
            .collect()
    }
}

/// Collection of server modules
pub type ServerModuleRegistry = ModuleRegistry<DynServerModule>;

pub trait ServerModuleRegistryExt {
    fn decoder_registry(&self) -> ModuleDecoderRegistry;
}

impl ServerModuleRegistryExt for ServerModuleRegistry {
    /// Generate a `ModuleDecoderRegistry` from this `ModuleRegistry`
    fn decoder_registry(&self) -> ModuleDecoderRegistry {
        // TODO: cache decoders
        self.iter_modules()
            .map(|(id, kind, module)| (id, kind.clone(), module.decoder()))
            .collect::<ModuleDecoderRegistry>()
    }
}
