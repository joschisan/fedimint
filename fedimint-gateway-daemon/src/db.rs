use fedimint_core::config::FederationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{impl_db_lookup, impl_db_record, secp256k1};
use fedimint_gateway_common::{FederationConfig, RegisteredProtocol};
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use secp256k1::Keypair;
use strum_macros::EnumIter;

#[repr(u8)]
#[derive(Clone, EnumIter, Debug)]
pub enum DbKeyPrefix {
    FederationConfig = 0x04,
    GatewayPublicKey = 0x06,
    RegisteredIncomingContract = 0x09,
    ClientDatabase = 0x10,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Encodable, Decodable)]
pub struct FederationConfigKeyPrefix;

#[derive(Debug, Clone, Encodable, Decodable, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct FederationConfigKey {
    pub id: FederationId,
}

impl_db_record!(
    key = FederationConfigKey,
    value = FederationConfig,
    db_prefix = DbKeyPrefix::FederationConfig,
);

impl_db_lookup!(
    key = FederationConfigKey,
    query_prefix = FederationConfigKeyPrefix
);

#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
pub struct GatewayPublicKey {
    pub protocol: RegisteredProtocol,
}

#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
pub struct GatewayPublicKeyPrefix;

impl_db_record!(
    key = GatewayPublicKey,
    value = Keypair,
    db_prefix = DbKeyPrefix::GatewayPublicKey,
);

impl_db_lookup!(
    key = GatewayPublicKey,
    query_prefix = GatewayPublicKeyPrefix
);

#[derive(Debug, Encodable, Decodable)]
pub struct RegisteredIncomingContractKey(pub PaymentImage);

#[derive(Debug, Encodable, Decodable)]
pub struct RegisteredIncomingContract {
    pub federation_id: FederationId,
    pub incoming_amount_msats: u64,
    pub contract: IncomingContract,
}

impl_db_record!(
    key = RegisteredIncomingContractKey,
    value = RegisteredIncomingContract,
    db_prefix = DbKeyPrefix::RegisteredIncomingContract,
);
