use fedimint_core::core::{ModuleKind, OperationId};
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_eventlog::{Event, EventKind, EventPersistence};
use serde::{Deserialize, Serialize};

const KIND: ModuleKind = fedimint_lnv2_common::KIND;

// --- Outgoing payment ---

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum SendPaymentStatus {
    /// Outgoing HTLC was claimed; carries the preimage.
    Success([u8; 32]),
    /// Outgoing payment was cancelled; carries the forfeit signature.
    Cancelled(Signature),
}

/// Event emitted when an outgoing payment reaches a final state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendPaymentUpdateEvent {
    pub operation_id: OperationId,
    pub status: SendPaymentStatus,
}

impl Event for SendPaymentUpdateEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("payment-send-update");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

// --- Incoming payment ---

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub enum ReceivePaymentStatus {
    Success([u8; 32]),
    Rejected,
    Refunded,
    Failure,
}

/// Event emitted when an incoming payment reaches a final state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceivePaymentUpdateEvent {
    pub operation_id: OperationId,
    pub status: ReceivePaymentStatus,
}

impl Event for ReceivePaymentUpdateEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("payment-receive-update");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}

// --- Complete (preimage revealed to LN network) ---

/// Event emitted when the completion state machine has settled or cancelled
/// the HTLC towards the LN node. Only applies to externally-routed receives.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct CompleteLightningPaymentEvent {
    pub operation_id: OperationId,
}

impl Event for CompleteLightningPaymentEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("complete-lightning-payment");
    const PERSISTENCE: EventPersistence = EventPersistence::Persistent;
}
