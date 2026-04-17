use std::collections::BTreeMap;

use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::{Amount, PeerId};
use serde::{Deserialize, Serialize};
use tbs::{AggregatePublicKey, PublicKeyShare};

use crate::Denomination;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MintConfig {
    pub private: MintConfigPrivate,
    pub consensus: MintConfigConsensus,
}

pub fn consensus_denominations() -> impl DoubleEndedIterator<Item = Denomination> {
    (0..42).map(Denomination)
}

pub fn client_denominations() -> impl DoubleEndedIterator<Item = Denomination> {
    (9..42).map(Denomination)
}

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct MintConfigConsensus {
    pub tbs_agg_pks: BTreeMap<Denomination, AggregatePublicKey>,
    pub tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, PublicKeyShare>>,
    pub input_fee: Amount,
    pub output_fee: Amount,
}

impl MintConfigConsensus {
    pub fn to_client(&self) -> MintClientConfig {
        MintClientConfig {
            tbs_agg_pks: self.tbs_agg_pks.clone(),
            tbs_pks: self.tbs_pks.clone(),
            input_fee: self.input_fee,
            output_fee: self.output_fee,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct MintConfigPrivate {
    pub tbs_sks: BTreeMap<Denomination, tbs::SecretKeyShare>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable, Hash)]
pub struct MintClientConfig {
    pub tbs_agg_pks: BTreeMap<Denomination, AggregatePublicKey>,
    pub tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, PublicKeyShare>>,
    pub input_fee: Amount,
    pub output_fee: Amount,
}

impl std::fmt::Display for MintClientConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "MintClientConfig {}",
            serde_json::to_string(self).map_err(|_e| std::fmt::Error)?
        )
    }
}
