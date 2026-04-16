use picomint_client_module::module::OutPointRange;
use picomint_core::core::ModuleKind;
use picomint_core::Amount;
use picomint_eventlog::{Event, EventKind};
use picomint_mint_common::KIND;
use serde::{Deserialize, Serialize};

/// Emitted when an output state machine reaches its terminal state. Used by
/// the `send` flow to wait for a reissuance bundle to finalise before
/// retrying note selection.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct OutputFinalisedEvent {
    pub range: OutPointRange,
}

impl Event for OutputFinalisedEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("output-finalised");
}

/// Event emitted when e-cash is sent out-of-band.
/// This is a final event - once e-cash is sent, the operation is complete.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendPaymentEvent {
    pub amount: Amount,
    pub ecash: String,
}

impl Event for SendPaymentEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("payment-send");
}

/// Event emitted when a receive (reissuance) operation is initiated.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceivePaymentEvent {
    pub amount: Amount,
}

impl Event for ReceivePaymentEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("payment-receive");
}

/// Status of a receive (reissuance) operation.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub enum ReceivePaymentStatus {
    /// The reissuance was successful.
    Success,
    /// The reissuance was rejected.
    Rejected,
}

/// Event emitted when a receive (reissuance) operation reaches a final state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceivePaymentUpdateEvent {
    pub status: ReceivePaymentStatus,
}

impl Event for ReceivePaymentUpdateEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("payment-receive-update");
}
