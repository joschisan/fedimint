use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::impl_db_record;

#[repr(u8)]
#[derive(Clone, Debug)]
pub enum DbKeyPrefix {
    SendStateMachine = 0x41,
    ReceiveStateMachine = 0x42,
    CompleteStateMachine = 0x43,
    Operation = 0x44,
}

/// Tracks that an operation has already been started for this
/// [`OperationId`]. Each gateway entry point (send/relay) derives its op id
/// deterministically from the contract, so we use this to reject duplicate
/// attempts.
#[derive(Debug, Encodable, Decodable)]
pub struct OperationKey(pub OperationId);

impl_db_record!(
    key = OperationKey,
    value = (),
    db_prefix = DbKeyPrefix::Operation,
);
