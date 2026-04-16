use fedimint_api_client::session_outcome::{AcceptedItem, SignedSessionOutcome};
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::{TransactionId, table};

table!(
    ACCEPTED_ITEM,
    u64 => AcceptedItem,
    "accepted-item",
);

table!(
    ACCEPTED_TRANSACTION,
    TransactionId => Vec<ModuleInstanceId>,
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
