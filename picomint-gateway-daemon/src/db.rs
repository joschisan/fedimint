use picomint_api_client::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::table;
use picomint_ln_common::contracts::{IncomingContract, PaymentImage};

table!(
    ROOT_ENTROPY,
    () => Vec<u8>,
    "root-entropy",
);

table!(
    CLIENT_CONFIG,
    FederationId => ConsensusConfig,
    "client-config",
);

table!(
    REGISTERED_INCOMING_CONTRACT,
    PaymentImage => RegisteredIncomingContract,
    "registered-incoming-contract",
);

#[derive(Debug, Encodable, Decodable)]
pub struct RegisteredIncomingContract {
    pub federation_id: FederationId,
    pub incoming_amount_msats: u64,
    pub contract: IncomingContract,
}

picomint_core::consensus_value!(RegisteredIncomingContract);
