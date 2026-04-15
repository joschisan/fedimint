//! redb-backed concrete implementation of the v2 database traits.
//!
//! The abstract contract — `TableDef`, the four `I*DatabaseTransactionOps*`
//! traits, the `table!` macro — lives in `fedimint-core::db::v2`. This file
//! owns the concrete tx and database types that back the server and modules.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::ops::Bound;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use fedimint_core::db::v2::{
    IReadDatabaseTransactionOps, IReadDatabaseTransactionOpsTyped as _,
    IWriteDatabaseTransactionOps, TableDef,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use tokio::sync::Notify;

// ─── Database ────────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct Database {
    inner: Arc<DatabaseInner>,
    prefix: Vec<String>,
}

struct DatabaseInner {
    env: redb::Database,
    /// Lazily-populated map of resolved table name -> shared `Notify`. Any
    /// commit that opened a table for write wakes every waiter on that table.
    notify: Mutex<BTreeMap<String, Arc<Notify>>>,
    decoders: OnceLock<ModuleDecoderRegistry>,
}

impl DatabaseInner {
    fn notify_for(&self, name: &str) -> Arc<Notify> {
        self.notify
            .lock()
            .expect("notify map poisoned")
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(Notify::new()))
            .clone()
    }
}

impl Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("prefix", &self.prefix)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Open (or create) a redb database at `path`. The only fallible entry
    /// point; every other public method panics internally on redb errors.
    pub async fn open(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let path = path.as_ref().to_owned();

        let env = tokio::task::spawn_blocking(move || redb::Database::create(path)).await??;

        Ok(Self {
            inner: Arc::new(DatabaseInner {
                env,
                notify: Mutex::new(BTreeMap::new()),
                decoders: OnceLock::new(),
            }),
            prefix: Vec::new(),
        })
    }

    /// Open an in-memory database. Intended for tests and ephemeral dev use.
    pub fn open_in_memory() -> Self {
        let env = redb::Database::builder()
            .create_with_backend(redb::backends::InMemoryBackend::new())
            .expect("in-memory redb create failed");

        Self {
            inner: Arc::new(DatabaseInner {
                env,
                notify: Mutex::new(BTreeMap::new()),
                decoders: OnceLock::new(),
            }),
            prefix: Vec::new(),
        }
    }

    /// Install the decoder registry used to deserialize dynamically-typed
    /// module values. Callable once (further calls are ignored).
    pub fn set_decoders(&self, decoders: ModuleDecoderRegistry) {
        let _ = self.inner.decoders.set(decoders);
    }

    /// Carve out a sub-namespace. Composable:
    /// `db.isolate("client_0").isolate("module_3")` produces tables named
    /// `client_0/module_3/<table>` on disk.
    pub fn isolate(&self, segment: impl Into<String>) -> Database {
        let mut prefix = self.prefix.clone();

        prefix.push(segment.into());

        Self {
            inner: self.inner.clone(),
            prefix,
        }
    }

    pub async fn begin_write(&self) -> WriteTransaction {
        let db = self.inner.clone();

        let tx = tokio::task::spawn_blocking(move || db.env.begin_write())
            .await
            .expect("spawn_blocking panicked")
            .expect("redb begin_write failed");

        WriteTransaction {
            tx,
            db: self.inner.clone(),
            prefix: self.prefix.clone(),
            touched: Mutex::new(BTreeSet::new()),
            on_commit: Mutex::new(Vec::new()),
        }
    }

    pub async fn begin_read(&self) -> ReadTransaction {
        let db = self.inner.clone();

        let tx = tokio::task::spawn_blocking(move || db.env.begin_read())
            .await
            .expect("spawn_blocking panicked")
            .expect("redb begin_read failed");

        ReadTransaction {
            tx,
            db: self.inner.clone(),
            prefix: self.prefix.clone(),
        }
    }

    /// Wait until `key` exists in `table`. Returns the value together with the
    /// [`ReadTransaction`] that observed it, so the caller can continue reading
    /// without opening a second tx.
    pub async fn wait_key<K, V>(&self, def: &TableDef<K, V>, key: &K) -> (V, ReadTransaction)
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.wait_key_check(def, key, |v| v).await
    }

    /// Wait until `check` on the current value returns `Some(T)`, then return
    /// `(T, ReadTransaction)`. The returned tx is the one that observed the
    /// matched state. `check` is called once on entry and again after every
    /// commit that touches `(table, key)`.
    pub async fn wait_key_check<K, V, T>(
        &self,
        def: &TableDef<K, V>,
        key: &K,
        mut check: impl FnMut(Option<V>) -> Option<T>,
    ) -> (T, ReadTransaction)
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let resolved = def.resolved_name(&self.prefix);

        let notify = self.inner.notify_for(&resolved);

        loop {
            let notified = notify.notified();

            let tx = self.begin_read().await;

            let current = tx.get(def, key);

            if let Some(t) = check(current) {
                return (t, tx);
            }

            drop(tx);

            notified.await;
        }
    }
}

// ─── Transactions ────────────────────────────────────────────────────────

pub struct ReadTransaction {
    tx: redb::ReadTransaction,
    db: Arc<DatabaseInner>,
    prefix: Vec<String>,
}

impl ReadTransaction {
    /// Borrow a view at this tx's root prefix.
    pub fn as_ref(&self) -> ReadTxRef<'_> {
        ReadTxRef {
            tx: &self.tx,
            db: &self.db,
            prefix: self.prefix.clone(),
        }
    }

    /// Borrow a view with an additional prefix segment.
    pub fn isolate(&self, segment: impl Into<String>) -> ReadTxRef<'_> {
        let mut view = self.as_ref();

        view.prefix.push(segment.into());

        view
    }
}

/// Borrowed view of a [`ReadTransaction`] with a possibly-extended prefix.
pub struct ReadTxRef<'tx> {
    tx: &'tx redb::ReadTransaction,
    db: &'tx Arc<DatabaseInner>,
    prefix: Vec<String>,
}

impl<'tx> ReadTxRef<'tx> {
    pub fn isolate(&self, segment: impl Into<String>) -> ReadTxRef<'tx> {
        let mut view = ReadTxRef {
            tx: self.tx,
            db: self.db,
            prefix: self.prefix.clone(),
        };

        view.prefix.push(segment.into());

        view
    }
}

pub struct WriteTransaction {
    tx: redb::WriteTransaction,
    db: Arc<DatabaseInner>,
    prefix: Vec<String>,
    /// Resolved names of tables opened for write during this tx. Populated any
    /// time a table is opened (including for read), used to notify waiters on
    /// commit. Over-notifies on pure reads — harmless but slightly noisy.
    touched: Mutex<BTreeSet<String>>,
    on_commit: Mutex<Vec<Box<dyn FnOnce() + Send + 'static>>>,
}

impl WriteTransaction {
    /// Borrow a view at this tx's root prefix.
    pub fn as_ref(&self) -> WriteTxRef<'_> {
        WriteTxRef {
            tx: &self.tx,
            db: &self.db,
            prefix: self.prefix.clone(),
            touched: &self.touched,
            on_commit: &self.on_commit,
        }
    }

    /// Borrow a view with an additional prefix segment. Used by the engine to
    /// hand a module-scoped view of a shared transaction to a server module.
    pub fn isolate(&self, segment: impl Into<String>) -> WriteTxRef<'_> {
        let mut view = self.as_ref();

        view.prefix.push(segment.into());

        view
    }

    /// Register a callback to run after a successful commit.
    pub fn on_commit(&self, f: impl FnOnce() + Send + 'static) {
        self.as_ref().on_commit(f);
    }

    pub async fn commit(self) {
        let Self {
            tx,
            db,
            touched,
            on_commit,
            ..
        } = self;

        tokio::task::spawn_blocking(move || tx.commit())
            .await
            .expect("spawn_blocking panicked")
            .expect("redb commit failed");

        for name in touched.into_inner().expect("touched poisoned") {
            db.notify_for(&name).notify_waiters();
        }

        for cb in on_commit.into_inner().expect("on_commit poisoned") {
            cb();
        }
    }
}

/// Borrowed view of a [`WriteTransaction`] with a possibly-extended prefix.
/// This is what server modules receive from the consensus engine; they cannot
/// commit, but they can read, write, isolate further, and register post-commit
/// callbacks that the owning [`WriteTransaction::commit`] will fire.
pub struct WriteTxRef<'tx> {
    tx: &'tx redb::WriteTransaction,
    db: &'tx Arc<DatabaseInner>,
    prefix: Vec<String>,
    touched: &'tx Mutex<BTreeSet<String>>,
    on_commit: &'tx Mutex<Vec<Box<dyn FnOnce() + Send + 'static>>>,
}

impl<'tx> WriteTxRef<'tx> {
    pub fn isolate(&self, segment: impl Into<String>) -> WriteTxRef<'tx> {
        let mut view = WriteTxRef {
            tx: self.tx,
            db: self.db,
            prefix: self.prefix.clone(),
            touched: self.touched,
            on_commit: self.on_commit,
        };

        view.prefix.push(segment.into());

        view
    }

    /// Register a callback to run after a successful commit.
    pub fn on_commit(&self, f: impl FnOnce() + Send + 'static) {
        self.on_commit
            .lock()
            .expect("on_commit poisoned")
            .push(Box::new(f));
    }
}

// ─── Bytes-level implementations ─────────────────────────────────────────

fn read_raw_get_bytes(
    tx: &redb::ReadTransaction,
    resolved_table: &str,
    key: &[u8],
) -> Option<Vec<u8>> {
    let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(resolved_table);

    let table = match tx.open_table(td) {
        Ok(t) => t,
        Err(redb::TableError::TableDoesNotExist(_)) => return None,
        Err(e) => panic!("redb open_table (read) failed: {e}"),
    };

    Some(table.get(key).expect("redb get failed")?.value().to_vec())
}

fn read_raw_iter_bytes(
    tx: &redb::ReadTransaction,
    resolved_table: &str,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(resolved_table);

    match tx.open_table(td) {
        Ok(t) => collect_iter(&t),
        Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
        Err(e) => panic!("redb open_table (read) failed: {e}"),
    }
}

fn read_raw_range_bytes(
    tx: &redb::ReadTransaction,
    resolved_table: &str,
    lo: Bound<Vec<u8>>,
    hi: Bound<Vec<u8>>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(resolved_table);

    match tx.open_table(td) {
        Ok(t) => collect_range(&t, lo, hi),
        Err(redb::TableError::TableDoesNotExist(_)) => Vec::new(),
        Err(e) => panic!("redb open_table (read) failed: {e}"),
    }
}

fn write_open_table<'tx>(
    tx: &'tx redb::WriteTransaction,
    touched: &Mutex<BTreeSet<String>>,
    resolved_table: &str,
) -> redb::Table<'tx, &'static [u8], &'static [u8]> {
    let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(resolved_table);

    let table = tx.open_table(td).expect("redb open_table (write) failed");

    touched
        .lock()
        .expect("touched poisoned")
        .insert(resolved_table.to_owned());

    table
}

fn collect_iter<T>(table: &T) -> Vec<(Vec<u8>, Vec<u8>)>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    table
        .iter()
        .expect("redb iter failed")
        .map(|r| {
            let (k, v) = r.expect("redb iter item failed");

            (k.value().to_vec(), v.value().to_vec())
        })
        .collect()
}

fn collect_range<T>(table: &T, lo: Bound<Vec<u8>>, hi: Bound<Vec<u8>>) -> Vec<(Vec<u8>, Vec<u8>)>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
{
    fn as_slice(b: &Bound<Vec<u8>>) -> Bound<&[u8]> {
        match b {
            Bound::Included(v) => Bound::Included(v.as_slice()),
            Bound::Excluded(v) => Bound::Excluded(v.as_slice()),
            Bound::Unbounded => Bound::Unbounded,
        }
    }

    table
        .range::<&[u8]>((as_slice(&lo), as_slice(&hi)))
        .expect("redb range failed")
        .map(|r| {
            let (k, v) = r.expect("redb range item failed");

            (k.value().to_vec(), v.value().to_vec())
        })
        .collect()
}

// — ReadTxRef —

impl IReadDatabaseTransactionOps for ReadTxRef<'_> {
    fn prefix(&self) -> &[String] {
        &self.prefix
    }

    fn decoders(&self) -> &OnceLock<ModuleDecoderRegistry> {
        &self.db.decoders
    }

    fn raw_get_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>> {
        read_raw_get_bytes(self.tx, resolved_table, key)
    }

    fn raw_iter_bytes(&self, resolved_table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        read_raw_iter_bytes(self.tx, resolved_table)
    }

    fn raw_range_bytes(
        &self,
        resolved_table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        read_raw_range_bytes(self.tx, resolved_table, lo, hi)
    }
}

// — ReadTransaction — owned type, delegates to the ref via the same helpers.

impl IReadDatabaseTransactionOps for ReadTransaction {
    fn prefix(&self) -> &[String] {
        &self.prefix
    }

    fn decoders(&self) -> &OnceLock<ModuleDecoderRegistry> {
        &self.db.decoders
    }

    fn raw_get_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>> {
        read_raw_get_bytes(&self.tx, resolved_table, key)
    }

    fn raw_iter_bytes(&self, resolved_table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        read_raw_iter_bytes(&self.tx, resolved_table)
    }

    fn raw_range_bytes(
        &self,
        resolved_table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        read_raw_range_bytes(&self.tx, resolved_table, lo, hi)
    }
}

// — WriteTxRef —

impl IReadDatabaseTransactionOps for WriteTxRef<'_> {
    fn prefix(&self) -> &[String] {
        &self.prefix
    }

    fn decoders(&self) -> &OnceLock<ModuleDecoderRegistry> {
        &self.db.decoders
    }

    fn raw_get_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>> {
        let table = write_open_table(self.tx, self.touched, resolved_table);

        Some(table.get(key).expect("redb get failed")?.value().to_vec())
    }

    fn raw_iter_bytes(&self, resolved_table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        let table = write_open_table(self.tx, self.touched, resolved_table);

        collect_iter(&table)
    }

    fn raw_range_bytes(
        &self,
        resolved_table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        let table = write_open_table(self.tx, self.touched, resolved_table);

        collect_range(&table, lo, hi)
    }
}

impl IWriteDatabaseTransactionOps for WriteTxRef<'_> {
    fn raw_insert_bytes(&self, resolved_table: &str, key: &[u8], value: &[u8]) -> Option<Vec<u8>> {
        let mut table = write_open_table(self.tx, self.touched, resolved_table);

        table
            .insert(key, value)
            .expect("redb insert failed")
            .map(|g| g.value().to_vec())
    }

    fn raw_remove_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>> {
        let mut table = write_open_table(self.tx, self.touched, resolved_table);

        table
            .remove(key)
            .expect("redb remove failed")
            .map(|g| g.value().to_vec())
    }

    fn raw_delete_table(&self, resolved_table: &str) {
        let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(resolved_table);

        self.tx.delete_table(td).expect("redb delete_table failed");

        self.touched
            .lock()
            .expect("touched poisoned")
            .insert(resolved_table.to_owned());
    }
}

// — WriteTransaction — reuses the WriteTxRef bytes impls via `as_ref()`.

impl IReadDatabaseTransactionOps for WriteTransaction {
    fn prefix(&self) -> &[String] {
        &self.prefix
    }

    fn decoders(&self) -> &OnceLock<ModuleDecoderRegistry> {
        &self.db.decoders
    }

    fn raw_get_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>> {
        self.as_ref().raw_get_bytes(resolved_table, key)
    }

    fn raw_iter_bytes(&self, resolved_table: &str) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.as_ref().raw_iter_bytes(resolved_table)
    }

    fn raw_range_bytes(
        &self,
        resolved_table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        self.as_ref().raw_range_bytes(resolved_table, lo, hi)
    }
}

impl IWriteDatabaseTransactionOps for WriteTransaction {
    fn raw_insert_bytes(&self, resolved_table: &str, key: &[u8], value: &[u8]) -> Option<Vec<u8>> {
        self.as_ref().raw_insert_bytes(resolved_table, key, value)
    }

    fn raw_remove_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>> {
        self.as_ref().raw_remove_bytes(resolved_table, key)
    }

    fn raw_delete_table(&self, resolved_table: &str) {
        self.as_ref().raw_delete_table(resolved_table);
    }
}

// ─── Playground tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use fedimint_core::db::v2::{
        IReadDatabaseTransactionOpsTyped as _, IWriteDatabaseTransactionOpsTyped as _, TableDef,
    };

    use super::*;

    const USERS: TableDef<u64, String> = TableDef::new("users");
    const BALANCES: TableDef<u64, u64> = TableDef::new("balances");

    #[tokio::test]
    async fn basic_read_write() {
        let db = Database::open_in_memory();

        let tx = db.begin_write().await;
        tx.insert(&USERS, &1, &"alice".to_string());
        tx.insert(&USERS, &2, &"bob".to_string());
        tx.insert(&BALANCES, &1, &100);
        tx.commit().await;

        let tx = db.begin_read().await;
        assert_eq!(tx.get(&USERS, &1), Some("alice".to_string()));
        assert_eq!(tx.get(&USERS, &2), Some("bob".to_string()));
        assert_eq!(tx.get(&USERS, &3), None);
        assert_eq!(tx.get(&BALANCES, &1), Some(100));
    }

    #[tokio::test]
    async fn uncommitted_writes_are_discarded() {
        let db = Database::open_in_memory();

        let tx = db.begin_write().await;
        tx.insert(&USERS, &1, &"alice".to_string());
        drop(tx);

        let tx = db.begin_read().await;
        assert_eq!(tx.get(&USERS, &1), None);
    }

    #[tokio::test]
    async fn isolation_separates_namespaces() {
        let db = Database::open_in_memory();
        let client_a = db.isolate("client_a");
        let client_b = db.isolate("client_b");

        let tx = client_a.begin_write().await;
        tx.insert(&USERS, &1, &"alice".to_string());
        tx.commit().await;

        let tx = client_b.begin_write().await;
        tx.insert(&USERS, &1, &"bob".to_string());
        tx.commit().await;

        assert_eq!(
            client_a.begin_read().await.get(&USERS, &1),
            Some("alice".to_string())
        );
        assert_eq!(
            client_b.begin_read().await.get(&USERS, &1),
            Some("bob".to_string())
        );
    }

    #[tokio::test]
    async fn nested_isolation_composes() {
        let db = Database::open_in_memory();
        let nested = db.isolate("gateway").isolate("client_7").isolate("mint");

        let tx = nested.begin_write().await;
        tx.insert(&BALANCES, &42, &999);
        tx.commit().await;

        assert_eq!(nested.begin_read().await.get(&BALANCES, &42), Some(999));
        assert_eq!(db.begin_read().await.get(&BALANCES, &42), None);
    }

    #[tokio::test]
    async fn range_iterates_sorted() {
        let db = Database::open_in_memory();

        let tx = db.begin_write().await;
        for i in 0u64..10 {
            tx.insert(&BALANCES, &i, &(i * 10));
        }
        tx.commit().await;

        let tx = db.begin_read().await;
        let items = tx.range(&BALANCES, 3u64..7u64);

        assert_eq!(items, vec![(3, 30), (4, 40), (5, 50), (6, 60)]);
    }

    #[tokio::test]
    async fn wait_key_wakes_after_commit() {
        let db = Database::open_in_memory();

        let db_writer = db.clone();
        let writer = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let tx = db_writer.begin_write().await;
            tx.insert(&USERS, &1, &"alice".to_string());
            tx.commit().await;
        });

        let (value, _tx) = db.wait_key(&USERS, &1).await;
        assert_eq!(value, "alice");

        writer.await.unwrap();
    }

    #[tokio::test]
    async fn wait_key_check_returns_consistent_tx() {
        let db = Database::open_in_memory();

        let tx = db.begin_write().await;
        tx.insert(&BALANCES, &1, &50);
        tx.commit().await;

        let db_writer = db.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            let tx = db_writer.begin_write().await;
            tx.insert(&BALANCES, &1, &150);
            tx.commit().await;
        });

        let (v, tx) = db
            .wait_key_check(&BALANCES, &1, |v| v.filter(|n| *n >= 100))
            .await;

        assert_eq!(v, 150);
        assert_eq!(tx.get(&BALANCES, &1), Some(150));
    }

    #[tokio::test]
    async fn on_commit_fires_after_commit() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let db = Database::open_in_memory();
        let fired = Arc::new(AtomicBool::new(false));

        let tx = db.begin_write().await;
        tx.insert(&USERS, &1, &"alice".to_string());
        let f = fired.clone();
        tx.on_commit(move || f.store(true, Ordering::SeqCst));
        assert!(!fired.load(Ordering::SeqCst));
        tx.commit().await;

        assert!(fired.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn shared_tx_with_module_isolation() {
        let db = Database::open_in_memory();

        let tx = db.begin_write().await;

        tx.insert(&USERS, &0, &"root".to_string());

        let m1 = tx.isolate("m1");
        m1.insert(&USERS, &1, &"alice".to_string());

        let m2 = tx.isolate("m2");
        m2.insert(&USERS, &1, &"bob".to_string());

        tx.commit().await;

        assert_eq!(db.begin_read().await.get(&USERS, &0), Some("root".into()));
        assert_eq!(
            db.begin_read().await.isolate("m1").get(&USERS, &1),
            Some("alice".into())
        );
        assert_eq!(
            db.begin_read().await.isolate("m2").get(&USERS, &1),
            Some("bob".into())
        );
    }

    #[tokio::test]
    async fn on_commit_does_not_fire_if_dropped() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let db = Database::open_in_memory();
        let fired = Arc::new(AtomicBool::new(false));

        let tx = db.begin_write().await;
        let f = fired.clone();
        tx.on_commit(move || f.store(true, Ordering::SeqCst));
        drop(tx);

        assert!(!fired.load(Ordering::SeqCst));
    }
}
