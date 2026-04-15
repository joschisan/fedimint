//! Abstract v2 database contract.
//!
//! Shape:
//!
//! - [`TableDef<K, V>`]: a typed table reference using consensus encoding for
//!   keys and values.
//! - [`IReadDatabaseTransactionOps`] / [`IWriteDatabaseTransactionOps`]:
//!   bytes-level primitives a concrete backend hand-implements (write is a
//!   supertrait of read).
//! - [`IReadDatabaseTransactionOpsTyped`] /
//!   [`IWriteDatabaseTransactionOpsTyped`]: typed ergonomics
//!   (`get`/`iter`/`range`/`insert`/`remove`/`delete_table`) provided as
//!   default methods and blanket-implemented over the bytes-level traits.
//!
//! The concrete redb-backed types (`Database`, `ReadTransaction`,
//! `WriteTransaction`, `ReadTxRef`, `WriteTxRef`) live in `fedimint-redb`.

use std::marker::PhantomData;
use std::ops::{Bound, RangeBounds};
use std::sync::OnceLock;

use crate::encoding::{Decodable, Encodable};
use crate::module::registry::ModuleDecoderRegistry;

// ─── Table definition ────────────────────────────────────────────────────

/// Typed table reference. `K` and `V` are stored on disk as
/// consensus-encoded bytes. The `name` is the logical table name; the physical
/// on-disk table name is the name joined with the owning transaction's
/// isolation prefix.
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

    /// Compute the on-disk table name under the given isolation prefix.
    /// Concrete backends call this when they need a resolved name outside of
    /// the typed-trait methods (e.g. to key a notification map).
    pub fn resolved_name(&self, prefix: &[String]) -> String {
        resolve_name(prefix, self.name)
    }
}

fn resolve_name(prefix: &[String], name: &str) -> String {
    prefix
        .iter()
        .map(String::as_str)
        .chain(std::iter::once(name))
        .collect::<Vec<_>>()
        .join("/")
}

/// Declare a typed [`TableDef`] constant.
///
/// Shape: ident, `K => V`, on-disk name literal, terminating comma.
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
        pub const $name: $crate::db::v2::TableDef<$k, $v> =
            $crate::db::v2::TableDef::new($label);
    };
}

// ─── Trait tower ─────────────────────────────────────────────────────────
//
// Mirrors the `migrate_to_redb_2` reference branch: a two-layer pair of
// traits (bytes-level plumbing, typed ergonomics), with the write traits
// supertraiting their read counterparts so anything accepting
// `&impl IReadDatabaseTransactionOpsTyped` works with both read and write tx
// types. Typed methods are default-impl'd and blanket-implemented over the
// bytes-level traits, so each tx type only hand-writes the four bytes
// methods.

/// Core raw read operations a database transaction supports.
pub trait IReadDatabaseTransactionOps {
    fn prefix(&self) -> &[String];

    fn decoders(&self) -> &OnceLock<ModuleDecoderRegistry>;

    fn raw_get_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>>;

    fn raw_iter_bytes(&self, resolved_table: &str) -> Vec<(Vec<u8>, Vec<u8>)>;

    fn raw_range_bytes(
        &self,
        resolved_table: &str,
        lo: Bound<Vec<u8>>,
        hi: Bound<Vec<u8>>,
    ) -> Vec<(Vec<u8>, Vec<u8>)>;
}

/// Write extension. Anything implementing this also reads.
pub trait IWriteDatabaseTransactionOps: IReadDatabaseTransactionOps {
    fn raw_insert_bytes(&self, resolved_table: &str, key: &[u8], value: &[u8]) -> Option<Vec<u8>>;

    fn raw_remove_bytes(&self, resolved_table: &str, key: &[u8]) -> Option<Vec<u8>>;

    fn raw_delete_table(&self, resolved_table: &str);
}

/// Typed read methods. Blanket-implemented for everything implementing
/// [`IReadDatabaseTransactionOps`]; users just `use` this trait.
pub trait IReadDatabaseTransactionOpsTyped: IReadDatabaseTransactionOps {
    fn get<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let table = resolve_name(self.prefix(), def.name);

        let raw = self.raw_get_bytes(&table, &key.consensus_encode_to_vec())?;

        Some(decode_value(&raw, self.decoders()))
    }

    fn iter<K, V>(&self, def: &TableDef<K, V>) -> Vec<(K, V)>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let table = resolve_name(self.prefix(), def.name);

        let decoders = self.decoders();

        self.raw_iter_bytes(&table)
            .into_iter()
            .map(|(k, v)| {
                (
                    decode_value::<K>(&k, decoders),
                    decode_value::<V>(&v, decoders),
                )
            })
            .collect()
    }

    fn range<K, V, R>(&self, def: &TableDef<K, V>, range: R) -> Vec<(K, V)>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
        R: RangeBounds<K>,
    {
        let table = resolve_name(self.prefix(), def.name);

        let decoders = self.decoders();

        let lo = match range.start_bound() {
            Bound::Included(k) => Bound::Included(k.consensus_encode_to_vec()),
            Bound::Excluded(k) => Bound::Excluded(k.consensus_encode_to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        };

        let hi = match range.end_bound() {
            Bound::Included(k) => Bound::Included(k.consensus_encode_to_vec()),
            Bound::Excluded(k) => Bound::Excluded(k.consensus_encode_to_vec()),
            Bound::Unbounded => Bound::Unbounded,
        };

        self.raw_range_bytes(&table, lo, hi)
            .into_iter()
            .map(|(k, v)| {
                (
                    decode_value::<K>(&k, decoders),
                    decode_value::<V>(&v, decoders),
                )
            })
            .collect()
    }
}

impl<T: IReadDatabaseTransactionOps + ?Sized> IReadDatabaseTransactionOpsTyped for T {}

/// Typed write methods. Blanket-implemented for everything implementing
/// [`IWriteDatabaseTransactionOps`].
pub trait IWriteDatabaseTransactionOpsTyped:
    IWriteDatabaseTransactionOps + IReadDatabaseTransactionOpsTyped
{
    fn insert<K, V>(&self, def: &TableDef<K, V>, key: &K, value: &V) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let table = resolve_name(self.prefix(), def.name);

        let prior = self.raw_insert_bytes(
            &table,
            &key.consensus_encode_to_vec(),
            &value.consensus_encode_to_vec(),
        )?;

        Some(decode_value(&prior, self.decoders()))
    }

    fn remove<K, V>(&self, def: &TableDef<K, V>, key: &K) -> Option<V>
    where
        K: Encodable + Decodable,
        V: Encodable + Decodable,
    {
        let table = resolve_name(self.prefix(), def.name);

        let prior = self.raw_remove_bytes(&table, &key.consensus_encode_to_vec())?;

        Some(decode_value(&prior, self.decoders()))
    }

    /// Drop the entire on-disk table for `def` at this view's prefix. After
    /// commit the table no longer exists; subsequent reads return empty.
    fn delete_table<K, V>(&self, def: &TableDef<K, V>) {
        let table = resolve_name(self.prefix(), def.name);

        self.raw_delete_table(&table);
    }
}

impl<T: IWriteDatabaseTransactionOps + ?Sized> IWriteDatabaseTransactionOpsTyped for T {}

fn decode_value<T: Decodable>(bytes: &[u8], decoders: &OnceLock<ModuleDecoderRegistry>) -> T {
    let d = decoders.get().cloned().unwrap_or_default();

    T::consensus_decode_whole(bytes, &d).expect("consensus_decode failed")
}
