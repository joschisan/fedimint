use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::impl_db_record;
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::util::SafeUrl;
use strum::EnumIter;

#[repr(u8)]
#[derive(Clone, EnumIter, Debug)]
pub enum DbKeyPrefix {
    Gateway = 0x41,
    IncomingContractStreamIndex = 0x42,
    SendStateMachine = 0x43,
    ReceiveStateMachine = 0x44,
    SendOperation = 0x45,
    #[allow(dead_code)]
    /// Prefixes between 0xb0..=0xcf shall all be considered allocated for
    /// historical and future external use
    ExternalReservedStart = 0xb0,
    #[allow(dead_code)]
    /// Prefixes between 0xd0..=0xff shall all be considered allocated for
    /// historical and future internal use
    CoreInternalReservedStart = 0xd0,
    #[allow(dead_code)]
    CoreInternalReservedEnd = 0xff,
}

#[derive(Debug, Encodable, Decodable)]
pub struct GatewayKey(pub PublicKey);

impl_db_record!(
    key = GatewayKey,
    value = SafeUrl,
    db_prefix = DbKeyPrefix::Gateway,
);

#[derive(Debug, Encodable, Decodable)]
pub struct IncomingContractStreamIndexKey;

impl_db_record!(
    key = IncomingContractStreamIndexKey,
    value = u64,
    db_prefix = DbKeyPrefix::IncomingContractStreamIndex
);

/// Tracks that a send operation has been started for this [`OperationId`].
/// Used to reject duplicate pay attempts for the same invoice (the op id is
/// derived from the invoice payment hash).
#[derive(Debug, Encodable, Decodable)]
pub struct SendOperationKey(pub OperationId);

impl_db_record!(
    key = SendOperationKey,
    value = (),
    db_prefix = DbKeyPrefix::SendOperation,
);
