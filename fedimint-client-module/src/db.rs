use fedimint_core::db::DatabaseTransaction;
use fedimint_core::util::BoxFuture;

/// A function that modules can implement to "migrate" the database to the next
/// database version.
pub type ClientModuleMigrationFn =
    for<'r, 'tx> fn(&'r mut DatabaseTransaction<'tx>) -> BoxFuture<'r, anyhow::Result<()>>;
