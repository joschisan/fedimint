use std::collections::BTreeMap;
use std::fmt::{Debug, Display};
use std::hash::Hash;
use std::path::Path;
use std::str::FromStr;

use bitcoin::hashes::{Hash as BitcoinHash, hex, sha256};
use hex::FromHex;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

use picomint_encoding::{Decodable, Encodable};

// TODO: make configurable
/// This limits the RAM consumption of a AlephBFT Unit to roughly 50kB
pub const ALEPH_BFT_UNIT_BYTE_LIMIT: usize = 50_000;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct PeerEndpoint {
    /// The peer's iroh API public key
    pub node_id: iroh_base::PublicKey,
    /// The peer's name
    pub name: String,
}

// FIXME: workaround for https://github.com/serde-rs/json/issues/989
pub fn de_int_key<'de, D, K, V>(deserializer: D) -> Result<BTreeMap<K, V>, D::Error>
where
    D: Deserializer<'de>,
    K: Eq + Ord + FromStr,
    K::Err: Display,
    V: Deserialize<'de>,
{
    let string_map = <BTreeMap<String, V>>::deserialize(deserializer)?;
    let map = string_map
        .into_iter()
        .map(|(key_str, value)| {
            let key = K::from_str(&key_str).map_err(serde::de::Error::custom)?;
            Ok((key, value))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    Ok(map)
}

/// The federation id is a copy of the authentication threshold public key of
/// the federation
///
/// Stable id so long as guardians membership does not change
/// Unique id so long as guardians do not all collude
#[derive(
    Debug,
    Copy,
    Serialize,
    Deserialize,
    Clone,
    Eq,
    Hash,
    PartialEq,
    Ord,
    PartialOrd,
    Encodable,
    Decodable,
)]
pub struct FederationId(pub sha256::Hash);

picomint_redb::consensus_key!(FederationId);

#[derive(
    Debug,
    Copy,
    Serialize,
    Deserialize,
    Clone,
    Eq,
    Hash,
    PartialEq,
    Encodable,
    Decodable,
    Ord,
    PartialOrd,
)]
/// Prefix of the [`FederationId`], useful for UX improvements
///
/// Intentionally compact to save on the encoding. With 4 billion
/// combinations real-life non-malicious collisions should never
/// happen.
pub struct FederationIdPrefix([u8; 4]);

impl Display for FederationIdPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&::hex::encode(self.0))
    }
}

impl Display for FederationId {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str(&::hex::encode(self.0.to_byte_array()))
    }
}

impl FromStr for FederationIdPrefix {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self(<[u8; 4]>::from_hex(s)?))
    }
}

impl FederationIdPrefix {
    pub fn to_bytes(&self) -> Vec<u8> {
        self.0.to_vec()
    }
}

/// Display as a hex encoding
impl FederationId {
    /// Random dummy id for testing
    pub fn dummy() -> Self {
        Self(sha256::Hash::from_byte_array([42; 32]))
    }

    pub(crate) fn from_byte_array(bytes: [u8; 32]) -> Self {
        Self(sha256::Hash::from_byte_array(bytes))
    }

    pub fn to_prefix(&self) -> FederationIdPrefix {
        FederationIdPrefix(self.0[..4].try_into().expect("can't fail"))
    }
}

impl FromStr for FederationId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(Self::from_byte_array(<[u8; 32]>::from_hex(s)?))
    }
}

/// Key under which the federation name can be sent to client in the `meta` part
/// of the config
pub const META_FEDERATION_NAME_KEY: &str = "federation_name";

pub fn load_from_file<T: DeserializeOwned>(path: &Path) -> Result<T, anyhow::Error> {
    let file = std::fs::File::open(path)?;
    Ok(serde_json::from_reader(file)?)
}
