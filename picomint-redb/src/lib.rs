//! redb-backed database for picomint.
//!
//! A [`NativeTableDef<K, V>`] is a typed table reference backed by redb's
//! native `TableDefinition<K, V>`. Keys implement `redb::Key + redb::Value`
//! directly; values implement `redb::Value` directly. Two per-type helper
//! macros:
//!
//! - [`consensus_value!`] — implements `redb::Value` via consensus encoding.
//! - [`consensus_key!`] — implements `redb::Key` + `redb::Value` via consensus
//!   encoding with byte-lex compare. Byte-lex compare matches numeric /
//!   lexicographic order because our encoding is fixed-width big-endian for
//!   integers and raw bytes for hashes.
//!
//! The concrete tx and database types (`Database`, `ReadTransaction`,
//! `WriteTransaction`, `ReadTxRef`, `WriteTxRef`) expose `insert`/`get`/
//! `remove`/`iter`/`range`/`delete_table` as inherent methods over
//! `NativeTableDef`.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::path::Path;
use std::sync::{Arc, Mutex};

pub use redb;
use redb::{ReadableDatabase, ReadableTable as _, TableDefinition};
use tokio::sync::Notify;

// ─── Per-type impl macros ─────────────────────────────────────────────────

/// Implement `redb::Value` for a type that already derives
/// `Encodable + Decodable`, serializing via consensus encoding.
///
/// ```ignore
/// #[derive(Debug, Encodable, Decodable)]
/// pub struct Foo { ... }
/// consensus_value!(Foo);
/// ```
#[macro_export]
macro_rules! consensus_value {
    ($ty:ty) => {
        impl $crate::redb::Value for $ty {
            type SelfType<'a>
                = $ty
            where
                Self: 'a;

            type AsBytes<'a>
                = ::std::vec::Vec<u8>
            where
                Self: 'a;

            fn fixed_width() -> ::std::option::Option<usize> {
                None
            }

            fn from_bytes<'a>(data: &'a [u8]) -> Self
            where
                Self: 'a,
            {
                <$ty as ::picomint_encoding::Decodable>::consensus_decode_exact(data)
                    .expect("consensus_decode failed")
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self) -> ::std::vec::Vec<u8>
            where
                Self: 'b,
            {
                <$ty as ::picomint_encoding::Encodable>::consensus_encode_to_vec(value)
            }

            fn type_name() -> $crate::redb::TypeName {
                $crate::redb::TypeName::new(concat!("picomint::", stringify!($ty)))
            }
        }
    };
}

/// Implement `redb::Key + redb::Value` for a type that already derives
/// `Encodable + Decodable`, serializing via consensus encoding with byte-lex
/// `compare` (fine for set-style lookup tables where we never range over a
/// semantic ordering of K).
///
/// ```ignore
/// #[derive(Debug, Encodable, Decodable)]
/// pub struct Foo(...);
/// consensus_key!(Foo);
/// ```
#[macro_export]
macro_rules! consensus_key {
    ($ty:ty) => {
        $crate::consensus_value!($ty);

        impl $crate::redb::Key for $ty {
            fn compare(data1: &[u8], data2: &[u8]) -> ::std::cmp::Ordering {
                data1.cmp(data2)
            }
        }
    };
}

// ─── NativeTableDef: redb-native typed table reference ───────────────────
//
// Typed table handle: `K` and `V` are real redb types (implement
// `redb::Key`/`redb::Value`). Gives direct access to redb's native typed
// `TableDefinition<K, V>` with zero bytes-level indirection.

pub struct NativeTableDef<K: redb::Key + 'static, V: redb::Value + 'static> {
    name: &'static str,
    _phantom: PhantomData<(K, V)>,
}

impl<K, V> NativeTableDef<K, V>
where
    K: redb::Key + 'static,
    V: redb::Value + 'static,
{
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            _phantom: PhantomData,
        }
    }

    pub fn resolved_name(&self, prefix: &[String]) -> String {
        prefix
            .iter()
            .map(String::as_str)
            .chain(std::iter::once(self.name))
            .collect::<Vec<_>>()
            .join("/")
    }
}

// ─── table! macro ────────────────────────────────────────────────────────

/// Declare a typed [`NativeTableDef`] constant.
///
/// Both `$k` and `$v` must already implement the relevant redb traits
/// directly — see `consensus_value!` and `consensus_key!`.
///
/// ```ignore
/// table!(
///     UNIX_TIME_VOTE,
///     PeerId => u64,
///     "unix-time-vote",
/// );
/// ```
#[macro_export]
macro_rules! table {
    (
        $(#[$attr:meta])*
        $name:ident,
        $k:ty => $v:ty,
        $label:literal $(,)?
    ) => {
        $(#[$attr])*
        pub const $name: $crate::NativeTableDef<$k, $v> =
            $crate::NativeTableDef::new($label);
    };
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
    /// Fires on every commit, regardless of which tables were written.
    global_notify: Arc<Notify>,
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
                global_notify: Arc::new(Notify::new()),
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
                global_notify: Arc::new(Notify::new()),
            }),
            prefix: Vec::new(),
        }
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
    pub async fn wait_key<WK, K, WV, V>(
        &self,
        def: &NativeTableDef<WK, WV>,
        key: &K,
    ) -> (V, ReadTransaction)
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.wait_key_check(def, key, |v| v).await
    }

    /// Notification future for the next commit on this database. Fires on
    /// every committed write, regardless of which tables were touched.
    ///
    /// Must be constructed *before* the check it guards — tokio's `Notified`
    /// captures the `notify_waiters` generation at construction time, so any
    /// commit that happens after `wait_commit()` returns but before the
    /// future is awaited will still wake the waiter. Wrapping this in an
    /// `async fn` would defer the generation capture to first poll and
    /// reintroduce the TOCTOU race.
    pub fn wait_commit(&self) -> tokio::sync::futures::Notified<'_> {
        self.inner.global_notify.notified()
    }

    /// Shared [`Notify`] handle for `table`'s resolved name (i.e. table name
    /// under this [`Database`]'s prefix). Fires via `notify_waiters` on every
    /// commit that opened the table for write. Callers should construct
    /// `notified()` *before* the check it guards (see [`Self::wait_commit`]).
    pub fn notify_for_table<WK, WV>(&self, def: &NativeTableDef<WK, WV>) -> Arc<Notify>
    where
        WK: redb::Key + 'static,
        WV: redb::Value + 'static,
    {
        self.inner.notify_for(&def.resolved_name(&self.prefix))
    }

    /// Wait until `check` on the current value returns `Some(T)`, then return
    /// `(T, ReadTransaction)`. The returned tx is the one that observed the
    /// matched state. `check` is called once on entry and again after every
    /// commit that touches `(table, key)`.
    pub async fn wait_key_check<WK, K, WV, V, T>(
        &self,
        def: &NativeTableDef<WK, WV>,
        key: &K,
        mut check: impl FnMut(Option<V>) -> Option<T>,
    ) -> (T, ReadTransaction)
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
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

        db.global_notify.notify_waiters();

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

    // ─── Native-typed ops (spike; will replace the bytes layer) ──────────
    //
    // Closure-based to sidestep GAT-related trait-resolution pain around
    // `redb::Value::SelfType<'a>`. The closure gets a real `redb::Table<'_>`
    // and can call `.insert`, `.get`, `.range`, `.iter` etc. directly.

    /// Open the native redb table at this view's prefix+name and hand it to
    /// `f`. Registers the table in `touched` for post-commit notification.
    pub fn with_native_table<K, V, R>(
        &self,
        def: &NativeTableDef<K, V>,
        f: impl FnOnce(&mut redb::Table<'_, K, V>) -> R,
    ) -> R
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        let resolved = def.resolved_name(&self.prefix);
        let td: TableDefinition<K, V> = TableDefinition::new(&resolved);
        let mut table = self
            .tx
            .open_table(td)
            .expect("redb open_table (write) failed");
        let r = f(&mut table);
        drop(table);
        self.touched
            .lock()
            .expect("touched poisoned")
            .insert(resolved);
        r
    }
}

impl ReadTxRef<'_> {
    /// Open the native redb table and hand it to `f`. Returns `None` if the
    /// table has never been written — treat that as "empty" at the call site.
    pub fn with_native_table<K, V, R>(
        &self,
        def: &NativeTableDef<K, V>,
        f: impl FnOnce(&redb::ReadOnlyTable<K, V>) -> R,
    ) -> Option<R>
    where
        K: redb::Key + 'static,
        V: redb::Value + 'static,
    {
        let resolved = def.resolved_name(&self.prefix);
        let td: TableDefinition<K, V> = TableDefinition::new(&resolved);
        match self.tx.open_table(td) {
            Ok(t) => Some(f(&t)),
            Err(redb::TableError::TableDoesNotExist(_)) => None,
            Err(e) => panic!("redb open_table (read) failed: {e}"),
        }
    }
}

// ─── Ergonomic typed ops over NativeTableDef<K, V> ───────────────────────
//
// HRTB `T: for<'a> redb::Value<SelfType<'a> = T>` is the "owned" contract —
// call sites hand in and receive back owned `T`, not borrowed slices.
// Satisfied by any type built with `consensus_value!` / `consensus_key!`
// and tuples thereof.

use std::ops::RangeBounds;

impl<'tx> WriteTxRef<'tx> {
    pub fn insert<WK, K, WV, V>(
        &self,
        def: &NativeTableDef<WK, WV>,
        key: &K,
        value: &V,
    ) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.with_native_table(def, |t| {
            t.insert(key, value)
                .expect("redb insert failed")
                .map(|g| g.value())
        })
    }

    pub fn remove<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.with_native_table(def, |t| {
            t.remove(key)
                .expect("redb remove failed")
                .map(|g| g.value())
        })
    }

    pub fn delete_table<WK, WV>(&self, def: &NativeTableDef<WK, WV>)
    where
        WK: redb::Key + 'static,
        WV: redb::Value + 'static,
    {
        let resolved = def.resolved_name(&self.prefix);
        let td: TableDefinition<WK, WV> = TableDefinition::new(&resolved);
        match self.tx.delete_table(td) {
            Ok(_) => {}
            Err(redb::TableError::TableDoesNotExist(_)) => {}
            Err(e) => panic!("redb delete_table failed: {e}"),
        }
        self.touched
            .lock()
            .expect("touched poisoned")
            .insert(resolved);
    }

    pub fn get<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.with_native_table(def, |t| {
            t.get(key).expect("redb get failed").map(|g| g.value())
        })
    }

    pub fn iter<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        use redb::ReadableTable as _;
        self.with_native_table(def, |t| {
            t.iter()
                .expect("redb iter failed")
                .map(|r| {
                    let (k, v) = r.expect("redb iter item failed");
                    (k.value(), v.value())
                })
                .collect()
        })
    }

    pub fn range<WK, K, WV, V, R>(&self, def: &NativeTableDef<WK, WV>, range: R) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
        R: RangeBounds<K>,
    {
        self.with_native_table(def, |t| {
            t.range::<&K>((range.start_bound(), range.end_bound()))
                .expect("redb range failed")
                .map(|r| {
                    let (k, v) = r.expect("redb range item failed");
                    (k.value(), v.value())
                })
                .collect()
        })
    }
}

impl ReadTxRef<'_> {
    pub fn get<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.with_native_table(def, |t| {
            t.get(key).expect("redb get failed").map(|g| g.value())
        })
        .flatten()
    }

    pub fn iter<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        use redb::ReadableTable as _;
        self.with_native_table(def, |t| {
            t.iter()
                .expect("redb iter failed")
                .map(|r| {
                    let (k, v) = r.expect("redb iter item failed");
                    (k.value(), v.value())
                })
                .collect()
        })
        .unwrap_or_default()
    }

    pub fn range<WK, K, WV, V, R>(&self, def: &NativeTableDef<WK, WV>, range: R) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
        R: RangeBounds<K>,
    {
        self.with_native_table(def, |t| {
            t.range::<&K>((range.start_bound(), range.end_bound()))
                .expect("redb range failed")
                .map(|r| {
                    let (k, v) = r.expect("redb range item failed");
                    (k.value(), v.value())
                })
                .collect()
        })
        .unwrap_or_default()
    }
}

// ─── DbRead / DbWrite trait abstraction ──────────────────────────────────
//
// Successor to the old `IReadDatabaseTransactionOps` trait tower. Typed
// methods are defined directly over `NativeTableDef<WK, WV>` and implemented
// on each concrete tx type. Server modules take `&impl DbRead` /
// `&impl DbWrite` to stay generic over owned-vs-borrowed and read-vs-write.

pub trait DbRead {
    fn get<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static;

    fn iter<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static;

    fn range<WK, K, WV, V, R>(&self, def: &NativeTableDef<WK, WV>, range: R) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
        R: RangeBounds<K>;
}

pub trait DbWrite: DbRead {
    fn insert<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K, value: &V) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static;

    fn remove<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static;

    fn delete_table<WK, WV>(&self, def: &NativeTableDef<WK, WV>)
    where
        WK: redb::Key + 'static,
        WV: redb::Value + 'static;
}

// Macro to cut the 3 read + 3 write method-body duplication per type.
// Each impl delegates to inherent methods that carry the actual logic.
macro_rules! impl_db_read_via_inherent {
    ($ty:ty) => {
        impl DbRead for $ty {
            fn get<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
            where
                WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
                WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
                K: Debug + 'static,
                V: Debug + 'static,
            {
                <$ty>::get(self, def, key)
            }

            fn iter<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>) -> Vec<(K, V)>
            where
                WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
                WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
                K: Debug + 'static,
                V: Debug + 'static,
            {
                <$ty>::iter(self, def)
            }

            fn range<WK, K, WV, V, R>(&self, def: &NativeTableDef<WK, WV>, range: R) -> Vec<(K, V)>
            where
                WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
                WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
                K: Debug + 'static,
                V: Debug + 'static,
                R: RangeBounds<K>,
            {
                <$ty>::range(self, def, range)
            }
        }
    };
}

macro_rules! impl_db_write_via_inherent {
    ($ty:ty) => {
        impl DbWrite for $ty {
            fn insert<WK, K, WV, V>(
                &self,
                def: &NativeTableDef<WK, WV>,
                key: &K,
                value: &V,
            ) -> Option<V>
            where
                WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
                WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
                K: Debug + 'static,
                V: Debug + 'static,
            {
                <$ty>::insert(self, def, key, value)
            }

            fn remove<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
            where
                WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
                WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
                K: Debug + 'static,
                V: Debug + 'static,
            {
                <$ty>::remove(self, def, key)
            }

            fn delete_table<WK, WV>(&self, def: &NativeTableDef<WK, WV>)
            where
                WK: redb::Key + 'static,
                WV: redb::Value + 'static,
            {
                <$ty>::delete_table(self, def)
            }
        }
    };
}

// ─── Owned-tx delegation ─────────────────────────────────────────────────
//
// Users commonly call `.insert/.get/...` directly on the owned
// `WriteTransaction`/`ReadTransaction` (not just on the borrowed
// `WriteTxRef`/`ReadTxRef`). These inherent impls delegate via `.as_ref()`.

impl ReadTransaction {
    pub fn get<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.as_ref().get(def, key)
    }

    pub fn iter<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.as_ref().iter(def)
    }

    pub fn range<WK, K, WV, V, R>(&self, def: &NativeTableDef<WK, WV>, range: R) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
        R: RangeBounds<K>,
    {
        self.as_ref().range(def, range)
    }
}

impl WriteTransaction {
    pub fn insert<WK, K, WV, V>(
        &self,
        def: &NativeTableDef<WK, WV>,
        key: &K,
        value: &V,
    ) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.as_ref().insert(def, key, value)
    }

    pub fn remove<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.as_ref().remove(def, key)
    }

    pub fn delete_table<WK, WV>(&self, def: &NativeTableDef<WK, WV>)
    where
        WK: redb::Key + 'static,
        WV: redb::Value + 'static,
    {
        self.as_ref().delete_table(def)
    }

    pub fn get<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>, key: &K) -> Option<V>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.as_ref().get(def, key)
    }

    pub fn iter<WK, K, WV, V>(&self, def: &NativeTableDef<WK, WV>) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
    {
        self.as_ref().iter(def)
    }

    pub fn range<WK, K, WV, V, R>(&self, def: &NativeTableDef<WK, WV>, range: R) -> Vec<(K, V)>
    where
        WK: for<'a> redb::Key<SelfType<'a> = K> + 'static,
        WV: for<'a> redb::Value<SelfType<'a> = V> + 'static,
        K: Debug + 'static,
        V: Debug + 'static,
        R: RangeBounds<K>,
    {
        self.as_ref().range(def, range)
    }
}

impl_db_read_via_inherent!(ReadTxRef<'_>);
impl_db_read_via_inherent!(WriteTxRef<'_>);
impl_db_read_via_inherent!(ReadTransaction);
impl_db_read_via_inherent!(WriteTransaction);
impl_db_write_via_inherent!(WriteTxRef<'_>);
impl_db_write_via_inherent!(WriteTransaction);

// ─── Playground tests ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    table!(USERS, u64 => String, "users");
    table!(BALANCES, u64 => u64, "balances");

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
