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

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct LightningConfigConsensus {
    pub tpe_agg_pk: AggregatePublicKey,
    pub tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
    pub input_fee: Amount,
    pub output_fee: Amount,
    pub network: Network,
}

impl LightningConfigConsensus {
    pub fn to_client(&self) -> LightningClientConfig {
        LightningClientConfig {
            tpe_agg_pk: self.tpe_agg_pk,
            tpe_pks: self.tpe_pks.clone(),
            input_fee: self.input_fee,
            output_fee: self.output_fee,
            network: self.network,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Encodable, Decodable)]
pub struct LightningConfigPrivate {
    pub sk: SecretKeyShare,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct LightningClientConfig {
    pub tpe_agg_pk: AggregatePublicKey,
    pub tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
    pub input_fee: Amount,
    pub output_fee: Amount,
    pub network: Network,
}

impl std::fmt::Display for LightningClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LightningClientConfig {self:?}")
    }
}
