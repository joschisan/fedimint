use picomint_core::TransactionId;
use picomint_core::session_outcome::{AcceptedItem, SignedSessionOutcome};
use picomint_redb::table;

table!(
    ACCEPTED_ITEM,
    u64 => AcceptedItem,
    "accepted-item",
);

table!(
    ACCEPTED_TRANSACTION,
    TransactionId => (),
    "accepted-transaction",
);

table!(
    SIGNED_SESSION_OUTCOME,
    u64 => SignedSessionOutcome,
    "signed-session-outcome",
);

table!(
    ALEPH_UNITS,
    u64 => Vec<u8>,
    "aleph-units",
);
