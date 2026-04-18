use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, Txid};
use picomint_core::TransactionId;
use picomint_core::core::ModuleKind;
use picomint_eventlog::{Event, EventKind};
use serde::{Deserialize, Serialize};

/// Emitted when a pegout (send to onchain) operation is initiated.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendEvent {
    pub txid: TransactionId,
    pub address: Address<NetworkUnchecked>,
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

impl Event for SendEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_core::wallet::KIND);
    const KIND: EventKind = EventKind::from_static("send");
}

/// Emitted when the pegout is observed on bitcoin with a confirmed txid.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendConfirmEvent {
    pub txid: Txid,
}

impl Event for SendConfirmEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_core::wallet::KIND);
    const KIND: EventKind = EventKind::from_static("send-confirm");
}

/// Emitted when the pegout fails to reach onchain confirmation.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct SendFailureEvent;

impl Event for SendFailureEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_core::wallet::KIND);
    const KIND: EventKind = EventKind::from_static("send-failure");
}

/// Emitted when a pegin operation is initiated.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReceiveEvent {
    pub txid: TransactionId,
    pub address: Address<NetworkUnchecked>,
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

impl Event for ReceiveEvent {
    const MODULE: Option<ModuleKind> = Some(picomint_core::wallet::KIND);
    const KIND: EventKind = EventKind::from_static("receive");
}
