use std::collections::BTreeMap;

use anyhow::Context;
use picomint_core::PeerId;
use picomint_core::config::{FederationId, META_FEDERATION_NAME_KEY, PeerEndpoint};
use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::module::CoreConsensusVersion;
use picomint_core::secp256k1::PublicKey;
use picomint_ln_common::config::LightningConfigConsensus;
use picomint_mint_common::config::MintConfigConsensus;
use picomint_wallet_common::config::WalletConfigConsensus;
use serde::{Deserialize, Serialize};

/// Federation-wide config. Produced by DKG on the server side, served to
/// clients via the [`CLIENT_CONFIG_ENDPOINT`], and stored in both the server
/// and client databases. Byte-for-byte identical on every peer.
///
/// [`CLIENT_CONFIG_ENDPOINT`]: picomint_core::endpoint_constants::CLIENT_CONFIG_ENDPOINT
#[derive(Debug, Clone, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
pub struct ConsensusConfig {
    /// Agreed on core consensus version
    pub version: CoreConsensusVersion,
    /// Public keys for the atomic broadcast to authenticate messages
    pub broadcast_public_keys: BTreeMap<PeerId, PublicKey>,
    /// Number of rounds per session
    pub broadcast_rounds_per_session: u16,
    /// Public keys + names for every peer's single iroh endpoint (p2p + api).
    pub iroh_endpoints: BTreeMap<PeerId, PeerEndpoint>,
    /// Free-form federation metadata (federation name, etc.)
    pub meta: BTreeMap<String, String>,
    /// Mint module config
    pub mint: MintConfigConsensus,
    /// Lightning module config
    pub ln: LightningConfigConsensus,
    /// Wallet module config
    pub wallet: WalletConfigConsensus,
}

picomint_core::consensus_value!(ConsensusConfig);

impl ConsensusConfig {
    pub fn calculate_federation_id(&self) -> FederationId {
        FederationId(self.iroh_endpoints.consensus_hash())
    }

    pub fn federation_name(&self) -> Option<&str> {
        self.meta.get(META_FEDERATION_NAME_KEY).map(String::as_str)
    }

    /// Get the value of a given meta field, attempting JSON decode and
    /// falling back to a raw-string read for the `String` target (some older
    /// meta values were stored unquoted).
    pub fn meta<V: serde::de::DeserializeOwned + 'static>(
        &self,
        key: &str,
    ) -> anyhow::Result<Option<V>> {
        let Some(str_value) = self.meta.get(key) else {
            return Ok(None);
        };
        let res = serde_json::from_str(str_value)
            .map(Some)
            .context(format!("Decoding meta field '{key}' failed"));

        if res.is_err() && std::any::TypeId::of::<V>() == std::any::TypeId::of::<String>() {
            let string_ret = Box::new(str_value.clone());
            let ret = unsafe {
                // V == String is checked above
                std::mem::transmute::<Box<String>, Box<V>>(string_ret)
            };
            Ok(Some(*ret))
        } else {
            res
        }
    }
}
