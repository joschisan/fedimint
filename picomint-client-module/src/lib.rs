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

use picomint_api_client::api::FederationApi;
pub use picomint_core::core::{ModuleKind, OperationId};
use picomint_core::{PeerId, TransactionId};
use picomint_eventlog::{Event, EventKind};
use picomint_redb::Database;
use serde::{Deserialize, Serialize};

pub use crate::module::ClientModule;

/// Environment variables
pub mod envs;
/// Per-module typed state machine executor
pub mod executor;
/// Module client interface definitions
pub mod module;
/// Secret handling & derivation
pub mod secret;
/// Structs and interfaces to construct Picomint transactions
pub mod transaction;

#[derive(Serialize, Deserialize)]
pub struct TxAcceptEvent {
    pub txid: TransactionId,
}

impl Event for TxAcceptEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-accept");
}

#[derive(Serialize, Deserialize)]
pub struct TxRejectEvent {
    pub txid: TransactionId,
    pub error: String,
}
impl Event for TxRejectEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-reject");
}

#[derive(Serialize, Deserialize)]
pub struct ModuleRecoveryStarted {
    pub kind: ModuleKind,
}

impl ModuleRecoveryStarted {
    pub fn new(kind: ModuleKind) -> Self {
        Self { kind }
    }
}

impl Event for ModuleRecoveryStarted {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("module-recovery-started");
}

#[derive(Serialize, Deserialize)]
pub struct ModuleRecoveryCompleted {
    pub kind: ModuleKind,
}

impl Event for ModuleRecoveryCompleted {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("module-recovery-completed");
}

/// Resources particular to a module instance
pub struct ClientModuleInstance<'m, M: ClientModule> {
    /// Module-specific DB
    pub db: Database,
    /// Module-specific API
    pub api: FederationApi,

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
