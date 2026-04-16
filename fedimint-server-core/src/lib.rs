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

use std::fmt::Debug;

use fedimint_core::core::{Decoder, ModuleInstanceId, ModuleKind};
use fedimint_core::module::audit::Audit;
use fedimint_core::module::{
    ApiEndpoint, CommonModuleInit, InputMeta, ModuleCommon, ModuleInit, TransactionItemAmounts,
};
use fedimint_core::{InPoint, OutPoint, PeerId, apply, async_trait_maybe_send};
use fedimint_redb::{ReadTxRef, WriteTxRef};
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
    /// other state changes trough transactions.
    async fn consensus_proposal(
        &self,
        dbtx: &ReadTxRef<'_>,
    ) -> Vec<<Self::Common as ModuleCommon>::ConsensusItem>;

    /// This function is called once for every consensus item. The function
    /// should return Ok if and only if the consensus item changes
    /// the system state. *Therefore this method should return an error in case
    /// of merely redundant consensus items such that they will be purged from
    /// the history of the federation.*
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
