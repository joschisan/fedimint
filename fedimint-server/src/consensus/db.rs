use std::collections::BTreeMap;

use fedimint_core::TransactionId;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::DatabaseVersion;
use fedimint_core::db::v2::TableDef;
use fedimint_core::session_outcome::{AcceptedItem, SignedSessionOutcome};
use fedimint_server_core::migration::DynServerDbMigrationFn;

pub const ACCEPTED_ITEM: TableDef<u64, AcceptedItem> = TableDef::new("accepted_item");

pub const ACCEPTED_TRANSACTION: TableDef<TransactionId, Vec<ModuleInstanceId>> =
    TableDef::new("accepted_transaction");

pub const SIGNED_SESSION_OUTCOME: TableDef<u64, SignedSessionOutcome> =
    TableDef::new("signed_session_outcome");

pub const ALEPH_UNITS: TableDef<u64, Vec<u8>> = TableDef::new("aleph_units");

pub fn get_global_database_migrations() -> BTreeMap<DatabaseVersion, DynServerDbMigrationFn> {
    BTreeMap::new()
}

/// Placeholder retained during the v2 migration — the old migration system
/// operated on v1 dbtx. Kept as a unit struct so existing wiring still
/// compiles; no v2 migrations are defined.
pub struct ServerDbMigrationContext;
