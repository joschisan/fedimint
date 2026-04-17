use std::collections::BTreeMap;

pub use bitcoin::Network;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::{Amount, PeerId};
use serde::{Deserialize, Serialize};
use tpe::{AggregatePublicKey, PublicKeyShare, SecretKeyShare};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LightningConfig {
    pub private: LightningConfigPrivate,
    pub consensus: LightningConfigConsensus,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct LightningConfigConsensus {
    pub tpe_agg_pk: AggregatePublicKey,
    pub tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
    pub input_fee: Amount,
    pub output_fee: Amount,
    pub network: Network,
}

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct LightningConfigPrivate {
    pub sk: SecretKeyShare,
}
