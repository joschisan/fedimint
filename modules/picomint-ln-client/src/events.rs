use picomint_core::Amount;
use picomint_core::core::ModuleKind;
use picomint_eventlog::{Event, EventKind};
use serde::{Deserialize, Serialize};

/// Event emitted when a send operation is created.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendPaymentEvent {
    pub amount: Amount,
    pub fee: Amount,
}

impl Event for SendPaymentEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("payment-send");
}

/// Status of a send operation.
#[derive(Serialize, Deserialize, Debug, Clone, Eq, PartialEq)]
pub enum SendPaymentStatus {
    /// The payment was successful, includes the preimage as proof of payment.
    Success([u8; 32]),
    /// The payment has been refunded.
    Refunded,
}

/// Event emitted when a send operation reaches a final state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendPaymentUpdateEvent {
    pub status: SendPaymentStatus,
}

impl Event for SendPaymentUpdateEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("payment-send-update");
}

/// Event emitted when a receive operation successfully completes and
/// transitions to the claiming state.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceivePaymentEvent {
    pub amount: Amount,
}

impl Event for ReceivePaymentEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("payment-receive");
}
