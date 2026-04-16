use core::fmt;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::io::Read;
use std::str::FromStr;

use anyhow::ensure;
use bech32::{Bech32m, Hrp};
use serde::{Deserialize, Serialize};

use crate::base32::FEDIMINT_PREFIX;
use crate::config::FederationId;
use crate::encoding::{Decodable, Encodable};
use crate::util::SafeUrl;
use crate::{NumPeersExt, PeerId};

/// Information required for client to join Federation
///
/// Can be used to download the configs and bootstrap a client.
///
/// ## Invariants
/// Constructors have to guarantee that:
///   * At least one Api entry is present
///   * At least one Federation ID is present
#[derive(Clone, Debug, Eq, PartialEq, Encodable, Hash, Ord, PartialOrd)]
pub struct InviteCode(Vec<InviteCodePart>);

impl Decodable for InviteCode {
    fn consensus_decode<R: Read>(r: &mut R) -> std::io::Result<Self> {
        let inner: Vec<InviteCodePart> = Decodable::consensus_decode(r)?;

        if !inner
            .iter()
            .any(|data| matches!(data, InviteCodePart::Api { .. }))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "No API was provided in the invite code",
            ));
        }

        if !inner
            .iter()
            .any(|data| matches!(data, InviteCodePart::FederationId(_)))
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "No Federation ID provided in invite code",
            ));
        }

        Ok(Self(inner))
    }
}

impl InviteCode {
    pub fn new(url: SafeUrl, peer: PeerId, federation_id: FederationId) -> Self {
        Self(vec![
            InviteCodePart::Api { url, peer },
            InviteCodePart::FederationId(federation_id),
        ])
    }

    pub fn from_map(
        peer_to_url_map: &BTreeMap<PeerId, SafeUrl>,
        federation_id: FederationId,
    ) -> Self {
        let max_size = peer_to_url_map.to_num_peers().max_evil() + 1;
        let mut code_vec: Vec<InviteCodePart> = peer_to_url_map
            .iter()
            .take(max_size)
            .map(|(peer, url)| InviteCodePart::Api {
                url: url.clone(),
                peer: *peer,
            })
            .collect();

        code_vec.push(InviteCodePart::FederationId(federation_id));

        Self(code_vec)
    }

    /// Constructs an [`InviteCode`] which contains as many guardian URLs as
    /// needed to always be able to join a working federation
    pub fn new_with_essential_num_guardians(
        peer_to_url_map: &BTreeMap<PeerId, SafeUrl>,
        federation_id: FederationId,
    ) -> Self {
        let max_size = peer_to_url_map.to_num_peers().max_evil() + 1;
        let mut code_vec: Vec<InviteCodePart> = peer_to_url_map
            .iter()
            .take(max_size)
            .map(|(peer, url)| InviteCodePart::Api {
                url: url.clone(),
                peer: *peer,
            })
            .collect();
        code_vec.push(InviteCodePart::FederationId(federation_id));

        Self(code_vec)
    }

    /// Returns the API URL of one of the guardians.
    pub fn url(&self) -> SafeUrl {
        self.0
            .iter()
            .find_map(|data| match data {
                InviteCodePart::Api { url, .. } => Some(url.clone()),
                _ => None,
            })
            .expect("Ensured by constructor")
    }

    /// Returns the id of the guardian from which we got the API URL, see
    /// [`InviteCode::url`].
    pub fn peer(&self) -> PeerId {
        self.0
            .iter()
            .find_map(|data| match data {
                InviteCodePart::Api { peer, .. } => Some(*peer),
                _ => None,
            })
            .expect("Ensured by constructor")
    }

    /// Get all peer URLs in the [`InviteCode`]
    pub fn peers(&self) -> BTreeMap<PeerId, SafeUrl> {
        self.0
            .iter()
            .filter_map(|entry| match entry {
                InviteCodePart::Api { url, peer } => Some((*peer, url.clone())),
                _ => None,
            })
            .collect()
    }

    /// Returns the federation's ID that can be used to authenticate the config
    /// downloaded from the API.
    pub fn federation_id(&self) -> FederationId {
        self.0
            .iter()
            .find_map(|data| match data {
                InviteCodePart::FederationId(federation_id) => Some(*federation_id),
                _ => None,
            })
            .expect("Ensured by constructor")
    }
}

/// For extendability [`InviteCode`] consists of parts, where client can ignore
/// ones they don't understand.
///
/// ones they don't understand Data that can be encoded in the invite code.
/// Currently we always just use one `Api` and one `FederationId` variant in an
/// invite code, but more can be added in the future while still keeping the
/// invite code readable for older clients, which will just ignore the new
/// fields.
#[derive(Clone, Debug, Eq, PartialEq, Encodable, Decodable, Hash, Ord, PartialOrd)]
enum InviteCodePart {
    /// API endpoint of one of the guardians
    Api {
        /// URL to reach an API that we can download configs from
        url: SafeUrl,
        /// Peer id of the host from the Url
        peer: PeerId,
    },

    /// Authentication id for the federation
    FederationId(FederationId),
}

/// We can represent client invite code as a bech32 string for compactness and
/// error-checking
///
/// Human readable part (HRP) includes the version
/// ```txt
/// [ hrp (4 bytes) ] [ id (48 bytes) ] ([ url len (2 bytes) ] [ url bytes (url len bytes) ])+
/// ```
const BECH32_HRP: Hrp = Hrp::parse_unchecked("fed1");

impl FromStr for InviteCode {
    type Err = anyhow::Error;

    fn from_str(encoded: &str) -> Result<Self, Self::Err> {
        if let Ok(invite_code) = crate::base32::decode_prefixed(FEDIMINT_PREFIX, encoded) {
            return Ok(invite_code);
        }

        let (hrp, data) = bech32::decode(encoded)?;

        ensure!(hrp == BECH32_HRP, "Invalid HRP in bech32 encoding");

        let invite = Self::consensus_decode_whole(&data)?;

        Ok(invite)
    }
}

/// Parses the invite code from a bech32 string
impl Display for InviteCode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let data = self.consensus_encode_to_vec();
        let encode = bech32::encode::<Bech32m>(BECH32_HRP, &data).map_err(|_| fmt::Error)?;
        formatter.write_str(&encode)
    }
}

impl Serialize for InviteCode {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        String::serialize(&self.to_string(), serializer)
    }
}

impl<'de> Deserialize<'de> for InviteCode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let string = Cow::<str>::deserialize(deserializer)?;
        Self::from_str(&string).map_err(serde::de::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use fedimint_core::PeerId;

    use crate::config::FederationId;
    use crate::invite_code::InviteCode;

    #[test]
    fn test_invite_code_to_from_string() {
        let parts = vec![
            crate::invite_code::InviteCodePart::Api {
                url: "wss://fedimintd.mplsfed.foo/".parse().expect("valid url"),
                peer: PeerId::new(0),
            },
            crate::invite_code::InviteCodePart::FederationId(FederationId(
                bitcoin::hashes::sha256::Hash::from_str(
                    "bea7ff4116f2b1d324c7b5d699cce4ac7408cee41db2c88027e21b76fff3b9f4",
                )
                .expect("valid hash"),
            )),
        ];
        let invite_code = InviteCode(parts.clone());

        let encoded = invite_code.to_string();
        let decoded = InviteCode::from_str(&encoded).expect("roundtrip parses");
        assert_eq!(decoded.0, parts);
    }
}
