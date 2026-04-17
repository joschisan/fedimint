use serde::Serialize;

use picomint_encoding::{Decodable, Encodable};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encodable, Decodable, Serialize)]
/// Connection information sent between peers in order to start config gen
pub struct PeerSetupCode {
    /// Name of the peer
    pub name: String,
    /// Public key of the peer's single iroh endpoint (serves both p2p and
    /// client-API traffic, demuxed by node-id on accept).
    pub pk: iroh_base::PublicKey,
    /// Federation name set by the leader
    pub federation_name: Option<String>,
    /// Total number of guardians (including the one who sets this), set by the
    /// leader
    pub federation_size: Option<u32>,
}
