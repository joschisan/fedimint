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
    pub fn open_table<K, V>(&self, def: &TableDef<K, V>) -> ReadTable<'_, K, V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let name = resolve_name(&self.prefix, def.name);

        let td: TableDefinition<&[u8], &[u8]> = TableDefinition::new(&name);

        let inner = self
            .tx
            .open_table(td)
            .expect("redb open_table (read) failed");

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
    pub fn open_table<K, V>(&self, def: &TableDef<K, V>) -> WriteTable<'_, K, V>
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

        self.touched
            .lock()
            .expect("touched poisoned")
            .insert(name);

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

// ─── Table handles ───────────────────────────────────────────────────────

pub struct ReadTable<'tx, K, V> {
    inner: redb::ReadOnlyTable<&'static [u8], &'static [u8]>,
    decoders: &'tx OnceLock<ModuleDecoderRegistry>,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> ReadTable<'_, K, V>
where
    K: Encodable + Decodable,
    V: Encodable + Decodable,
{
    pub fn get(&self, key: &K) -> Option<V> {
        let k = key.consensus_encode_to_vec();

        let raw = self.inner.get(k.as_slice()).expect("redb get failed")?;

        Some(decode_value(raw.value(), self.decoders))
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

        let prior = self
            .inner
            .remove(k.as_slice())
            .expect("redb remove failed");

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

