use crate::p2p::ReconnectP2PConnections;
use fedimint_core::{NumPeers, PeerId};

use crate::p2p::P2PMessage;

/// A handle passed to `ServerModuleInit::distributed_gen`
///
/// This struct encapsulates dkg data that the module should not have a direct
/// access to, and implements higher level dkg operations available to the
/// module to complete its distributed initialization inside the federation.
#[non_exhaustive]
pub struct PeerHandle<'a> {
    // TODO: this whole type should be a part of a `fedimint-server` and fields here inaccessible
    // to outside crates, but until `ServerModule` is not in `fedimint-server` this is impossible
    #[doc(hidden)]
    pub num_peers: NumPeers,
    #[doc(hidden)]
    pub identity: PeerId,
    #[doc(hidden)]
    pub connections: &'a ReconnectP2PConnections<P2PMessage>,
}

impl<'a> PeerHandle<'a> {
    pub fn new(
        num_peers: NumPeers,
        identity: PeerId,
        connections: &'a ReconnectP2PConnections<P2PMessage>,
    ) -> Self {
        Self {
            num_peers,
            identity,
            connections,
        }
    }

    pub fn num_peers(&self) -> NumPeers {
        self.num_peers
    }
}
