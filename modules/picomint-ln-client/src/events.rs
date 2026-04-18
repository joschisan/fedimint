use picomint_core::Amount;
use picomint_core::TransactionId;
use picomint_core::core::ModuleKind;
use picomint_eventlog::{Event, EventKind};
use serde::{Deserialize, Serialize};

/// Emitted when a send operation is created.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendEvent {
    pub txid: TransactionId,
    pub amount: Amount,
    pub fee: Amount,
}

impl Event for SendEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("send");
}

/// Emitted when the payment successfully resolves and the preimage is known.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendSuccessEvent {
    pub preimage: [u8; 32],
}

impl Event for SendSuccessEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("send-success");
}

/// Emitted when the payment fails and funds are refunded via a new claim tx.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendRefundEvent {
    pub txid: TransactionId,
}

impl Event for SendRefundEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("send-refund");
}

/// Emitted when a receive operation successfully claims the incoming contract.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceiveEvent {
    pub txid: TransactionId,
    pub amount: Amount,
}

impl Event for ReceiveEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("receive");
}

/// Emitted when the incoming contract expires before a payment arrives.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceiveExpiryEvent;

impl Event for ReceiveExpiryEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_ln_common::KIND);
    const KIND: EventKind = EventKind::from_static("receive-expiry");
}
