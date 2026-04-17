use std::collections::BTreeMap;
use std::io::Write as _;

use bitcoin::hashes::{Hash, sha256};
use picomint_encoding::{Decodable, Encodable};
use picomint_core::{NumPeersExt as _, PeerId, secp256k1};
use secp256k1::{Message, PublicKey, SECP256K1};

use crate::transaction::ConsensusItem;

/// A consensus item accepted in the consensus
///
/// If two correct nodes obtain two ordered items from the broadcast they
/// are guaranteed to be in the same order. However, an ordered items is
/// only guaranteed to be seen by all correct nodes if a correct node decides to
/// accept it.
#[derive(Clone, Debug, PartialEq, Eq, Encodable, Decodable)]
pub struct AcceptedItem {
    pub item: ConsensusItem,
    pub peer: PeerId,
}

picomint_redb::consensus_value!(AcceptedItem);

/// Items ordered in a single session that have been accepted by Picomint
/// consensus.
///
/// A running Federation produces a [`SessionOutcome`] every couple of minutes.
/// Therefore, just like in Bitcoin, a [`SessionOutcome`] might be empty if no
/// items are ordered in that time or all ordered items are discarded by
/// Picomint Consensus.
///
/// When session is closed it is signed over by the peers and produces a
/// [`SignedSessionOutcome`].
#[derive(Clone, Debug, PartialEq, Eq, Encodable, Decodable)]
pub struct SessionOutcome {
    pub items: Vec<AcceptedItem>,
}

impl SessionOutcome {
    /// A blocks header consists of 40 bytes formed by its index in big endian
    /// bytes concatenated with the merkle root build from the consensus
    /// hashes of its [`AcceptedItem`]s or 32 zero bytes if the block is
    /// empty. The use of a merkle tree allows for efficient inclusion
    /// proofs of accepted consensus items for clients.
    pub fn header(&self, index: u64) -> [u8; 40] {
        let mut header = [0; 40];

        header[..8].copy_from_slice(&index.to_be_bytes());

        let leaf_hashes = self
            .items
            .iter()
            .map(Encodable::consensus_hash::<sha256::Hash>);

        if let Some(root) = bitcoin::merkle_tree::calculate_root(leaf_hashes) {
            header[8..].copy_from_slice(&root.to_byte_array());
        } else {
            assert!(self.items.is_empty());
        }

        header
    }
}

/// A [`SessionOutcome`], signed by the Federation.
///
/// A signed block combines a block with the naive threshold secp schnorr
/// signature for its header created by the federation. The signed blocks allow
/// clients and recovering guardians to verify the federations consensus
/// history. After a signed block has been created it is stored in the database.
#[derive(Clone, Debug, Encodable, Decodable, Eq, PartialEq)]
pub struct SignedSessionOutcome {
    pub session_outcome: SessionOutcome,
    pub signatures: std::collections::BTreeMap<PeerId, secp256k1::schnorr::Signature>,
}

picomint_redb::consensus_value!(SignedSessionOutcome);

impl SignedSessionOutcome {
    pub fn verify(
        &self,
        broadcast_public_keys: &BTreeMap<PeerId, PublicKey>,
        block_index: u64,
    ) -> bool {
        let message = {
            let mut engine = sha256::HashEngine::default();
            engine
                .write_all(broadcast_public_keys.consensus_hash_sha256().as_ref())
                .expect("Writing to a hash engine can not fail");
            engine
                .write_all(&self.session_outcome.header(block_index))
                .expect("Writing to a hash engine can not fail");
            Message::from_digest(sha256::Hash::from_engine(engine).to_byte_array())
        };

        let threshold = broadcast_public_keys.to_num_peers().threshold();
        if self.signatures.len() < threshold {
            return false;
        }

        self.signatures.iter().all(|(peer_id, signature)| {
            let Some(pub_key) = broadcast_public_keys.get(peer_id) else {
                return false;
            };

            SECP256K1
                .verify_schnorr(signature, &message, &pub_key.x_only_public_key().0)
                .is_ok()
        })
    }
}
