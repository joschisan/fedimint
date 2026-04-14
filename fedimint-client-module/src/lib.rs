#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::explicit_deref_methods)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::needless_lifetimes)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::type_complexity)]

use std::ops::{self};

use fedimint_api_client::api::DynModuleApi;
pub use fedimint_core::core::{IInput, IOutput, ModuleInstanceId, ModuleKind, OperationId};
use fedimint_core::db::Database;
use fedimint_core::{PeerId, TransactionId, maybe_add_send_sync};
use fedimint_eventlog::{Event, EventKind, EventPersistence};
use serde::{Deserialize, Serialize};
use transaction::{ClientInputBundle, ClientOutput};

pub use crate::module::ClientModule;
use crate::transaction::{ClientInput, ClientOutputBundle};

pub mod db;

/// Environment variables
pub mod envs;
/// Per-module typed state machine executor
pub mod executor;
/// Module client interface definitions
pub mod module;
/// Secret handling & derivation
pub mod secret;
/// Structs and interfaces to construct Fedimint transactions
pub mod transaction;

#[derive(Serialize, Deserialize)]
pub struct TxCreatedEvent {
    pub txid: TransactionId,
    pub operation_id: OperationId,
}

impl Event for TxCreatedEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-created");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

#[derive(Serialize, Deserialize)]
pub struct TxAcceptedEvent {
    pub txid: TransactionId,
    pub operation_id: OperationId,
}

impl Event for TxAcceptedEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-accepted");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

#[derive(Serialize, Deserialize)]
pub struct TxRejectedEvent {
    pub txid: TransactionId,
    pub error: String,
    pub operation_id: OperationId,
}
impl Event for TxRejectedEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-rejected");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

#[derive(Serialize, Deserialize)]
pub struct ModuleRecoveryStarted {
    module_id: ModuleInstanceId,
}

impl ModuleRecoveryStarted {
    pub fn new(module_id: ModuleInstanceId) -> Self {
        Self { module_id }
    }
}

impl Event for ModuleRecoveryStarted {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("module-recovery-started");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

#[derive(Serialize, Deserialize)]
pub struct ModuleRecoveryCompleted {
    pub module_id: ModuleInstanceId,
}

impl Event for ModuleRecoveryCompleted {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("module-recovery-completed");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

pub type InstancelessDynClientInput = ClientInput<Box<maybe_add_send_sync!(dyn IInput + 'static)>>;

pub type InstancelessDynClientInputBundle =
    ClientInputBundle<Box<maybe_add_send_sync!(dyn IInput + 'static)>>;

pub type InstancelessDynClientOutput =
    ClientOutput<Box<maybe_add_send_sync!(dyn IOutput + 'static)>>;

pub type InstancelessDynClientOutputBundle =
    ClientOutputBundle<Box<maybe_add_send_sync!(dyn IOutput + 'static)>>;

/// Resources particular to a module instance
pub struct ClientModuleInstance<'m, M: ClientModule> {
    /// Instance id of the module
    pub id: ModuleInstanceId,
    /// Module-specific DB
    pub db: Database,
    /// Module-specific API
    pub api: DynModuleApi,

    pub module: &'m M,
}

impl<'m, M: ClientModule> ClientModuleInstance<'m, M> {
    /// Get a reference to the module
    pub fn inner(&self) -> &'m M {
        self.module
    }
}

impl<M> ops::Deref for ClientModuleInstance<'_, M>
where
    M: ClientModule,
{
    type Target = M;

    fn deref(&self) -> &Self::Target {
        self.module
    }
}
#[derive(Deserialize)]
pub struct GetInviteCodeRequest {
    pub peer: PeerId,
}
