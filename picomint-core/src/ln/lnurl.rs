use bitcoin::secp256k1::PublicKey;
use picomint_encoding::{Decodable, Encodable};

use crate::config::FederationId;
use crate::util::SafeUrl;
use serde::{Deserialize, Serialize};
use tpe::AggregatePublicKey;

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct LnurlRequest {
    pub federation_id: FederationId,
    pub recipient_pk: PublicKey,
    pub aggregate_pk: AggregatePublicKey,
    pub gateways: Vec<SafeUrl>,
}
