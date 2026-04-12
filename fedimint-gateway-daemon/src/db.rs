use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{impl_db_lookup, impl_db_record};
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};

#[repr(u8)]
#[derive(Clone, Debug)]
pub enum DbKeyPrefix {
    RootEntropy = 0x00,
    ClientDatabase = 0x01,
    ClientConfig = 0x02,
    RegisteredIncomingContract = 0x03,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

// Root entropy for mnemonic
#[derive(Clone, Debug, Encodable, Decodable)]
pub struct RootEntropyKey;

impl_db_record!(
    key = RootEntropyKey,
    value = Vec<u8>,
    db_prefix = DbKeyPrefix::RootEntropy,
);

// Per-federation client config
#[derive(Clone, Debug, Encodable, Decodable)]
pub struct ClientConfigKey(pub FederationId);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct ClientConfigPrefix;

impl_db_record!(
    key = ClientConfigKey,
    value = ClientConfig,
    db_prefix = DbKeyPrefix::ClientConfig,
);

impl_db_lookup!(key = ClientConfigKey, query_prefix = ClientConfigPrefix);

// Registered incoming LNv2 contracts
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
