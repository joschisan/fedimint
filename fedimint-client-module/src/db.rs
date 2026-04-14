use fedimint_core::db::WriteDatabaseTransaction;
use fedimint_core::util::BoxFuture;

/// A function that modules can implement to "migrate" the database to the next
/// database version.
pub type ClientModuleMigrationFn =
    for<'r, 'tx> fn(&'r mut WriteDatabaseTransaction<'tx>) -> BoxFuture<'r, anyhow::Result<()>>;
