use serde::Serialize;

use crate::encoding::{Decodable, Encodable};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encodable, Decodable, Serialize)]
/// Connection information sent between peers in order to start config gen
pub struct PeerSetupCode {
    /// Name of the peer
    pub name: String,
    /// The peer's api and p2p endpoint
    pub endpoints: PeerEndpoints,
    /// Federation name set by the leader
    pub federation_name: Option<String>,
    /// Total number of guardians (including the one who sets this), set by the
    /// leader
    pub federation_size: Option<u32>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encodable, Decodable, Serialize)]
pub struct PeerEndpoints {
    /// Public key for our iroh api endpoint
    pub api_pk: iroh_base::PublicKey,
    /// Public key for our iroh p2p endpoint
    pub p2p_pk: iroh_base::PublicKey,
}
