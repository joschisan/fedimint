use picomint_core::Amount;
use picomint_core::TransactionId;
use picomint_core::core::ModuleKind;
use picomint_eventlog::{Event, EventKind};
use picomint_core::mint::KIND;
use serde::{Deserialize, Serialize};

/// Emitted when e-cash is sent out-of-band.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendEvent {
    pub amount: Amount,
    pub ecash: String,
}

impl Event for SendEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("send");
}

/// Emitted when a send operation requires reissuing notes before the sender
/// has enough of the right denominations to send.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReissueEvent {
    pub txid: TransactionId,
}

impl Event for ReissueEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("reissue");
}

/// Emitted when a receive (reissuance) operation is initiated.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceiveEvent {
    pub txid: TransactionId,
    pub amount: Amount,
}

impl Event for ReceiveEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("receive");
}

/// Emitted when an issuance state machine successfully finalises new notes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub(crate) struct IssuanceComplete {
    pub txid: TransactionId,
}

impl Event for IssuanceComplete {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("issuance-complete");
}

/// Emitted when an output state machine fails to finalise notes.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct OutputFailureEvent;

impl Event for OutputFailureEvent {
    const MODULE: Option<ModuleKind> = Some(KIND);
    const KIND: EventKind = EventKind::from_static("output-failure");
}
