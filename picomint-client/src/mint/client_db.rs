use std::collections::{BTreeMap, BTreeSet};

use bitcoin_hashes::hash160;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::table;

use super::SpendableNote;
use super::issuance::NoteIssuanceRequest;

// Tracks that a `receive(ecash)` has been started for this deterministic
// [`OperationId`]. Used to make `receive` idempotent.
table!(
    RECEIVE_OPERATION,
    OperationId => (),
    "receive-operation",
);

table!(
    NOTE,
    SpendableNote => (),
    "note",
);

/// Recovery state that can be checkpointed and resumed
#[derive(Debug, Clone, Encodable, Decodable)]
pub struct RecoveryState {
    /// Next item index to download
    pub next_index: u64,
    /// Total items (for progress calculation)
    pub total_items: u64,
    /// Already recovered note requests, keyed by `nonce_hash` (for efficient
    /// removal when inputs are seen)
    pub requests: BTreeMap<hash160::Hash, NoteIssuanceRequest>,
    /// Nonces seen (to detect duplicates)
    pub nonces: BTreeSet<hash160::Hash>,
}

picomint_redb::consensus_value!(RecoveryState);

table!(
    RECOVERY_STATE,
    () => RecoveryState,
    "recovery-state",
);
