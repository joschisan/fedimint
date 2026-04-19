use std::collections::BTreeMap;

use bitcoin::hashes::{Hash, sha256};
use picomint_encoding::{Decodable, Encodable};
use secp256k1::{Message, PublicKey, SECP256K1};

use crate::transaction::ConsensusItem;
use crate::{NumPeersExt as _, PeerId, secp256k1};

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
    /// A block header pairs its index with the merkle root built from the
    /// consensus hashes of its [`AcceptedItem`]s. The merkle tree allows for
    /// efficient inclusion proofs of accepted consensus items for clients.
    /// Empty sessions have no root.
    pub fn header(&self, index: u64) -> (u64, Option<sha256::Hash>) {
        let leaves = self
            .items
            .iter()
            .map(Encodable::consensus_hash::<sha256::Hash>);
        (index, bitcoin::merkle_tree::calculate_root(leaves))
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
        let message = Message::from_digest(
            (
                broadcast_public_keys.consensus_hash::<sha256::Hash>(),
                self.session_outcome.header(block_index),
            )
                .consensus_hash::<sha256::Hash>()
                .to_byte_array(),
        );

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
