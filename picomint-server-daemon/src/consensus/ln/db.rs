use picomint_core::ln::contracts::{IncomingContract, OutgoingContract};
use picomint_core::util::SafeUrl;
use picomint_core::{OutPoint, PeerId};
use picomint_redb::table;
use tpe::DecryptionKeyShare;

table!(
    BLOCK_COUNT_VOTE,
    PeerId => u64,
    "block-count-vote",
);

table!(
    UNIX_TIME_VOTE,
    PeerId => u64,
    "unix-time-vote",
);

table!(
    INCOMING_CONTRACT,
    OutPoint => IncomingContract,
    "incoming-contract",
);

table!(
    OUTGOING_CONTRACT,
    OutPoint => OutgoingContract,
    "outgoing-contract",
);

table!(
    DECRYPTION_KEY_SHARE,
    OutPoint => DecryptionKeyShare,
    "decryption-key-share",
);

table!(
    PREIMAGE,
    OutPoint => [u8; 32],
    "preimage",
);

table!(
    GATEWAY,
    SafeUrl => (),
    "gateway",
);

// Incoming contracts are indexed in three ways:
// 1) A sequential stream: `stream_index (u64)` -> `(OutPoint, IncomingContract)`
//    for efficient streaming reads via range queries on
//    `INCOMING_CONTRACT_STREAM`.
// 2) A monotonically-increasing index (`INCOMING_CONTRACT_STREAM_INDEX` -> u64)
//    that stores the next stream index to assign, used to wait for new incoming
//    contracts.
// 3) A reverse lookup `OutPoint` -> `stream_index` via
//    `INCOMING_CONTRACT_INDEX`, used to remove a contract from the stream once
//    it has been spent.
table!(
    INCOMING_CONTRACT_STREAM_INDEX,
    () => u64,
    "incoming-contract-stream-index",
);

table!(
    INCOMING_CONTRACT_STREAM,
    u64 => (OutPoint, IncomingContract),
    "incoming-contract-stream",
);

table!(
    INCOMING_CONTRACT_INDEX,
    OutPoint => u64,
    "incoming-contract-index",
);
