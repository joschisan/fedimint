use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::table;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};

table!(
    ROOT_ENTROPY,
    () => Vec<u8>,
    "root-entropy",
);

table!(
    CLIENT_CONFIG,
    FederationId => ClientConfig,
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

fedimint_core::consensus_value!(RegisteredIncomingContract);
