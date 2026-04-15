use std::collections::BTreeMap;

use fedimint_core::TransactionId;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::DatabaseVersion;
use fedimint_core::session_outcome::{AcceptedItem, SignedSessionOutcome};
use fedimint_core::table;
use fedimint_server_core::migration::DynServerDbMigrationFn;

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

pub fn get_global_database_migrations() -> BTreeMap<DatabaseVersion, DynServerDbMigrationFn> {
    BTreeMap::new()
}

/// Placeholder retained during the v2 migration — the old migration system
/// operated on v1 dbtx. Kept as a unit struct so existing wiring still
/// compiles; no v2 migrations are defined.
pub struct ServerDbMigrationContext;
