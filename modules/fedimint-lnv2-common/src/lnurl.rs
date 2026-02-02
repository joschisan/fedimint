use bitcoin::secp256k1::PublicKey;
use fedimint_core::config::FederationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::util::SafeUrl;
use serde::{Deserialize, Serialize};
use tpe::AggregatePublicKey;

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct LnurlRequest {
    pub federation_id: FederationId,
    pub recipient_pk: PublicKey,
    pub aggregate_pk: AggregatePublicKey,
    pub gateways: Vec<SafeUrl>,
}
