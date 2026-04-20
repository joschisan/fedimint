use async_trait::async_trait;
use picomint_redb::Database;
use tracing::info;

use crate::LOG_CONSENSUS;
use crate::consensus::db::ALEPH_UNITS;

pub struct BackupReader {
    db: Database,
}

impl BackupReader {
    pub fn new(db: Database) -> Self {
        Self { db }
    }
}

#[async_trait]
impl aleph_bft::BackupReader for BackupReader {
    async fn read(&mut self) -> std::io::Result<Vec<u8>> {
        let tx = self.db.begin_read();

        let units: Vec<Vec<u8>> = tx.iter(&ALEPH_UNITS, |r| r.map(|(_, v)| v).collect());

        if !units.is_empty() {
            info!(target: LOG_CONSENSUS, units_len = %units.len(), "Recovering from an in-session-shutdown");
        }

        Ok(units.into_iter().flatten().collect())
    }
}

pub struct BackupWriter {
    db: Database,
    units_index: u64,
}

impl BackupWriter {
    pub async fn new(db: Database) -> Self {
        let units_index = db
            .begin_read()
            .iter(&ALEPH_UNITS, |r| r.next_back().map_or(0, |(k, _)| k + 1));

        Self { db, units_index }
    }
}

#[async_trait]
impl aleph_bft::BackupWriter for BackupWriter {
    async fn append(&mut self, data: &[u8]) -> std::io::Result<()> {
        let tx = self.db.begin_write();

        tx.insert(&ALEPH_UNITS, &self.units_index, &data.to_owned());

        self.units_index += 1;

        tx.commit();

        Ok(())
    }
}
