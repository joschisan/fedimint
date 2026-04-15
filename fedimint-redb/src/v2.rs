//! New experimental database stack.
//!
//! Clean-slate redesign that collapses the layered `IRawDatabase` / `IDatabase`
//! / typed-transaction tower into:
//!
//! - [`Database`]: a single type. Isolation is a `Vec<String>` of prefix
//!   segments; nest with [`Database::isolate`].
//! - [`WriteTransaction`] / [`ReadTransaction`]: owned handles; caller commits.
//! - [`TableDef<K, V>`]: a typed table reference using consensus encoding for
//!   keys and values.
//!
//! All public methods panic on redb errors. Only [`Database::open`] is
//! fallible. All tables notify on write; [`Database::wait_key`] /
//! [`Database::wait_key_check`] wake after the enclosing `WriteTransaction` is
//! committed.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::ops::RangeBounds;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};

use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleDecoderRegistry;
use redb::{ReadableDatabase, ReadableTable, TableDefinition};
use tokio::sync::Notify;

// ─── Table definition ────────────────────────────────────────────────────

/// Typed table reference. `K` and `V` are stored on disk as
/// consensus-encoded bytes. The `name` is the logical table name; the physical
/// redb table name is the name prefixed with the owning [`Database`]'s
/// isolation path.
pub struct TableDef<K, V> {
    name: &'static str,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> TableDef<K, V> {
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _phantom: PhantomData,
        }
    }
}

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
    /// `client_0_module_3_<table>` on disk.
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
        let resolved = resolve_name(&self.prefix, def.name);

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

fn resolve_name(prefix: &[String], name: &str) -> String {
    prefix
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(name))
        .collect::<Vec<_>>()
        .join("_")
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

    pub fn open_table<K, V>(&self, def: &TableDef<K, V>) -> ReadTable<'_, K, V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.as_ref().open_table(def)
    }

    pub fn get<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.as_ref().get(def, key)
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

    pub fn open_table<K, V>(&self, def: &TableDef<K, V>) -> ReadTable<'tx, K, V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let name = resolve_name(&self.prefix, def.name);

        let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&name);

        let inner = match self.tx.open_table(td) {
            Ok(t) => Some(t),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => panic!("redb open_table (read) failed: {e}"),
        };

        ReadTable {
            inner,
            decoders: &self.db.decoders,
            _phantom: PhantomData,
        }
    }

    pub fn get<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.open_table(def).get(key)
    }
}

pub struct WriteTransaction {
    tx: redb::WriteTransaction,
    db: Arc<DatabaseInner>,
    prefix: Vec<String>,
    /// Resolved names of tables opened for write during this tx. Populated at
    /// `open_table` time (once per table), used to notify waiters on commit.
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

    pub fn open_table<K, V>(&self, def: &TableDef<K, V>) -> WriteTable<'_, K, V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.as_ref().open_table(def)
    }

    pub fn get<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.as_ref().get(def, key)
    }

    pub fn insert<K, V>(&self, def: &TableDef<K, V>, key: &K, value: &V) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.as_ref().insert(def, key, value)
    }

    pub fn remove<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.as_ref().remove(def, key)
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

    pub fn open_table<K, V>(&self, def: &TableDef<K, V>) -> WriteTable<'tx, K, V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let name = resolve_name(&self.prefix, def.name);

        let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&name);

        let inner = self
            .tx
            .open_table(td)
            .expect("redb open_table (write) failed");

        self.touched.lock().expect("touched poisoned").insert(name);

        WriteTable {
            inner,
            decoders: &self.db.decoders,
            _phantom: PhantomData,
        }
    }

    pub fn get<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.open_table(def).get(key)
    }

    pub fn insert<K, V>(&self, def: &TableDef<K, V>, key: &K, value: &V) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.open_table(def).insert(key, value)
    }

    pub fn remove<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        self.open_table(def).remove(key)
    }

    /// Register a callback to run after a successful commit.
    pub fn on_commit(&self, f: impl FnOnce() + Send + 'static) {
        self.on_commit
            .lock()
            .expect("on_commit poisoned")
            .push(Box::new(f));
    }
}

// ─── Table handles ───────────────────────────────────────────────────────

pub struct ReadTable<'tx, K, V> {
    inner: Option<redb::ReadOnlyTable<&'static [u8], &'static [u8]>>,
    decoders: &'tx OnceLock<ModuleDecoderRegistry>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> ReadTable<'_, K, V>
where
    K: Encodable + Decodable,
    V: Encodable + Decodable,
{
    pub fn get(&self, key: &K) -> Option<V> {
        let inner = self.inner.as_ref()?;

        let k = key.consensus_encode_to_vec();

        let raw = inner.get(k.as_slice()).expect("redb get failed")?;

        Some(decode_value(raw.value(), self.decoders))
    }

    pub fn range<R>(&self, range: R) -> Vec<(K, V)>
    where
        R: RangeBounds<K>,
    {
        match &self.inner {
            Some(t) => range_collect(t, range, self.decoders),
            None => Vec::new(),
        }
    }

    pub fn iter(&self) -> Vec<(K, V)> {
        let Some(inner) = &self.inner else {
            return Vec::new();
        };

        inner
            .iter()
            .expect("redb iter failed")
            .map(|r| {
                let (k, v) = r.expect("redb iter item failed");

                (
                    decode_value::<K>(k.value(), self.decoders),
                    decode_value::<V>(v.value(), self.decoders),
                )
            })
            .collect()
    }
}

pub struct WriteTable<'tx, K, V> {
    inner: redb::Table<'tx, &'static [u8], &'static [u8]>,
    decoders: &'tx OnceLock<ModuleDecoderRegistry>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> WriteTable<'_, K, V>
where
    K: Encodable + Decodable,
    V: Encodable + Decodable,
{
    pub fn get(&self, key: &K) -> Option<V> {
        let k = key.consensus_encode_to_vec();

        let raw = self.inner.get(k.as_slice()).expect("redb get failed")?;

        Some(decode_value(raw.value(), self.decoders))
    }

    pub fn insert(&mut self, key: &K, value: &V) -> Option<V> {
        let k = key.consensus_encode_to_vec();

        let v = value.consensus_encode_to_vec();

        let prior = self
            .inner
            .insert(k.as_slice(), v.as_slice())
            .expect("redb insert failed");

        prior.map(|g| decode_value(g.value(), self.decoders))
    }

    pub fn remove(&mut self, key: &K) -> Option<V> {
        let k = key.consensus_encode_to_vec();

        let prior = self.inner.remove(k.as_slice()).expect("redb remove failed");

        prior.map(|g| decode_value(g.value(), self.decoders))
    }

    pub fn range<R>(&self, range: R) -> Vec<(K, V)>
    where
        R: RangeBounds<K>,
    {
        range_collect(&self.inner, range, self.decoders)
    }

    pub fn iter(&self) -> Vec<(K, V)> {
        self.inner
            .iter()
            .expect("redb iter failed")
            .map(|r| {
                let (k, v) = r.expect("redb iter item failed");

                (
                    decode_value::<K>(k.value(), self.decoders),
                    decode_value::<V>(v.value(), self.decoders),
                )
            })
            .collect()
    }
}

fn range_collect<T, K, V, R>(
    table: &T,
    range: R,
    decoders: &OnceLock<ModuleDecoderRegistry>,
) -> Vec<(K, V)>
where
    T: ReadableTable<&'static [u8], &'static [u8]>,
    K: Encodable + Decodable,
    V: Encodable + Decodable,
    R: RangeBounds<K>,
{
    use std::ops::Bound;

    let encode_bound = |b: Bound<&K>| -> Bound<Vec<u8>> {
        match b {
            Bound::Included(k) => Bound::Included(k.consensus_encode_to_vec()),
            Bound::Excluded(k) => Bound::Excluded(k.consensus_encode_to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        }
    };

    let lo = encode_bound(range.start_bound());

    let hi = encode_bound(range.end_bound());

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

            (
                decode_value::<K>(k.value(), decoders),
                decode_value::<V>(v.value(), decoders),
            )
        })
        .collect()
}

fn decode_value<T: Decodable>(bytes: &[u8], decoders: &OnceLock<ModuleDecoderRegistry>) -> T {
    let d = decoders.get().cloned().unwrap_or_default();

    T::consensus_decode_whole(bytes, &d).expect("consensus_decode failed")
}

// ─── Playground tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::time::Duration;

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
        let table = tx.open_table(&BALANCES);
        let items = table.range(3u64..7u64);

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
