use std::collections::BTreeMap;

use fedimint_core::core::ModuleKind;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::serde_json;
use fedimint_core::{plugin_types_trait_impl_config, Amount, PeerId};
use serde::{Deserialize, Serialize};
use tbs::{AggregatePublicKey, PublicKeyShare};

use crate::{Denomination, MintCommonInit};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintGenParams {
    pub input_fee: Amount,
    pub output_fee: Amount,
}

pub fn consensus_denominations() -> impl DoubleEndedIterator<Item = Denomination> {
    (0..42).map(Denomination)
}

pub fn client_denominations() -> impl DoubleEndedIterator<Item = Denomination> {
    (9..42).map(Denomination)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MintConfig {
    pub private: MintConfigPrivate,
    pub consensus: MintConfigConsensus,
}

#[derive(Clone, Debug, Serialize, Deserialize, Encodable, Decodable)]
pub struct MintConfigConsensus {
    pub tbs_agg_pks: BTreeMap<Denomination, AggregatePublicKey>,
    pub tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, PublicKeyShare>>,
    pub input_fee: Amount,
    pub output_fee: Amount,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
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

// Wire together the configs for this module
plugin_types_trait_impl_config!(
    MintCommonInit,
    MintConfig,
    MintConfigPrivate,
    MintConfigConsensus,
    MintClientConfig
);
