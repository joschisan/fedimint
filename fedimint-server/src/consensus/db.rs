use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::v2::TableDef;
use fedimint_core::session_outcome::{AcceptedItem, SignedSessionOutcome};
use fedimint_core::TransactionId;

pub const ACCEPTED_ITEM: TableDef<u64, AcceptedItem> = TableDef::new("accepted_item");

pub const ACCEPTED_TRANSACTION: TableDef<TransactionId, Vec<ModuleInstanceId>> =
    TableDef::new("accepted_transaction");

pub const SIGNED_SESSION_OUTCOME: TableDef<u64, SignedSessionOutcome> =
    TableDef::new("signed_session_outcome");

pub const ALEPH_UNITS: TableDef<u64, Vec<u8>> = TableDef::new("aleph_units");
