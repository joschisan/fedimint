use std::path::Path;

use anyhow::{Context as _, Result};
use fedimint_db_locked::{Locked, LockedBuilder};
use redb::Database;

use crate::MemAndRedb;

impl MemAndRedb {
    pub async fn new(db_path: impl AsRef<Path>) -> Result<Locked<MemAndRedb>> {
        let db_path = db_path.as_ref();
        fedimint_core::task::block_in_place(|| Self::open_blocking(db_path))
    }

    fn open_blocking(db_path: &Path) -> Result<Locked<MemAndRedb>> {
        std::fs::create_dir_all(
            db_path
                .parent()
                .ok_or_else(|| anyhow::anyhow!("db path must have a base dir"))?,
        )?;
        LockedBuilder::new(db_path)?.with_db(|| {
            let db = match Database::create(db_path) {
                Ok(db) => db,
                Err(redb::DatabaseError::UpgradeRequired(_)) => {
                    Self::migrate_v2_to_v3(db_path)?;
                    Database::create(db_path)
                        .context("Failed to open redb database after v2->v3 migration")?
                }
                Err(e) => {
                    return Err(anyhow::Error::from(e))
                        .context("Failed to create/open redb database");
                }
            };
            Self::new_from_redb(db).map_err(|e| anyhow::Error::msg(e.to_string()))
        })
    }

    fn migrate_v2_to_v3(db_path: &Path) -> Result<()> {
        tracing::info!("Migrating redb database from v2 to v3 file format");
        let mut old_db = redb2::Database::open(db_path)
            .context("Failed to open redb v2 database for migration")?;
        old_db
            .upgrade()
            .context("Failed to upgrade redb database to v3 format")?;
        tracing::info!("Successfully migrated redb database to v3 format");
        Ok(())
    }
}
