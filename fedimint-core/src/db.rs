//! Abstract v2 database contract.
//!
//! A [`NativeTableDef<K, V>`] is a typed table reference backed by redb's
//! native `TableDefinition<K, V>`. Values are wrapped in either
//! [`Borsh<V>`] (new borsh path) or [`Consensus<V>`] (bridge over the legacy
//! Encodable/Decodable path); keys use either a direct `redb::Key` impl
//! (e.g. via [`redb_newtype_key!`]) or the [`ConsensusKey<T>`] bridge.
//!
//! The concrete redb-backed tx types (`Database`, `ReadTransaction`,
//! `WriteTransaction`, `ReadTxRef`, `WriteTxRef`) live in `fedimint-redb` and
//! expose `insert`/`get`/`remove`/`iter`/`range`/`delete_table` as inherent
//! methods over `NativeTableDef`.

use std::cell::Cell;
use std::fmt::Debug;
use std::marker::PhantomData;

use borsh::{BorshDeserialize, BorshSerialize};

use crate::encoding::{Decodable, Encodable};
use crate::module::registry::ModuleDecoderRegistry;

// ─── Decoder scope ───────────────────────────────────────────────────────
//
// redb's `Value::from_bytes` is a static method with no channel for passing
// the module decoder registry. `ConsensusKey<T>` / `Consensus<V>` need it to
// decode dynamic module types (e.g. `AcceptedItem`). Callers install the
// registry via [`with_decoders`] for the duration of a redb op; the wrappers
// read it from this thread-local. The raw pointer never outlives the scope
// because redb ops are sync and we restore on drop.

thread_local! {
    static CURRENT_DECODERS: Cell<*const ModuleDecoderRegistry> =
        const { Cell::new(std::ptr::null()) };
}

/// Install `decoders` as the decoder registry visible to `Consensus`/
/// `ConsensusKey` `from_bytes` for the duration of `f`. Restores the prior
/// registry on return.
pub fn with_decoders<R>(decoders: &ModuleDecoderRegistry, f: impl FnOnce() -> R) -> R {
    let prev = CURRENT_DECODERS.with(|c| c.replace(decoders as *const _));

    struct Restore(*const ModuleDecoderRegistry);
    impl Drop for Restore {
        fn drop(&mut self) {
            CURRENT_DECODERS.with(|c| c.set(self.0));
        }
    }
    let _restore = Restore(prev);

    f()
}

fn decode_with_current_scope<T: Decodable>(data: &[u8]) -> T {
    let ptr = CURRENT_DECODERS.with(Cell::get);
    if ptr.is_null() {
        T::consensus_decode_whole(data, &ModuleDecoderRegistry::default())
            .expect("consensus_decode failed")
    } else {
        // SAFETY: `with_decoders` keeps the registry alive for the duration
        // of `f`, and redb ops are synchronous — we never read the pointer
        // after the scope returns.
        T::consensus_decode_whole(data, unsafe { &*ptr }).expect("consensus_decode failed")
    }
}

// ─── Borsh<T>: redb::Value adapter ───────────────────────────────────────
//
// Any `BorshSerialize + BorshDeserialize` type becomes a redb `Value` by
// wrapping it in `Borsh<T>` **in the table definition only** —
// `SelfType<'a> = T` keeps the wrapper invisible at call sites. Variable
// width; not a `Key` (borsh integers are LE, which don't sort lexicographically
// — use `redb_newtype_key!` for ordered primitive-newtype keys instead).

#[derive(Debug)]
pub struct Borsh<T>(PhantomData<T>);

impl<T> redb::Value for Borsh<T>
where
    T: BorshSerialize + BorshDeserialize + Debug + 'static,
{
    type SelfType<'a>
        = T
    where
        Self: 'a;

    type AsBytes<'a>
        = Vec<u8>
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> T
    where
        Self: 'a,
    {
        borsh::from_slice(data).expect("borsh decode failed")
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a T) -> Vec<u8>
    where
        Self: 'b,
    {
        borsh::to_vec(value).expect("borsh encode can't fail")
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new(&format!("fedimint::Borsh<{}>", std::any::type_name::<T>()))
    }
}

// ─── ConsensusKey<T>: redb::Key adapter over Encodable/Decodable ─────────
//
// Companion to `Consensus<V>`: `ConsensusKey<T>` makes any Encodable+Decodable
// type usable as a redb key via consensus encoding. Sorts bytes lexicographic-
// ally — fine for set-style lookup tables where we never need to range over
// a semantic ordering of K. Planned to be deleted alongside `Consensus<V>`
// once every K has a direct `redb::Key` impl.

#[derive(Debug)]
pub struct ConsensusKey<T>(PhantomData<T>);

impl<T> redb::Value for ConsensusKey<T>
where
    T: Encodable + Decodable + Debug + 'static,
{
    type SelfType<'a>
        = T
    where
        Self: 'a;

    type AsBytes<'a>
        = Vec<u8>
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> T
    where
        Self: 'a,
    {
        decode_with_current_scope::<T>(data)
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a T) -> Vec<u8>
    where
        Self: 'b,
    {
        value.consensus_encode_to_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new(&format!(
            "fedimint::ConsensusKey<{}>",
            std::any::type_name::<T>()
        ))
    }
}

impl<T> redb::Key for ConsensusKey<T>
where
    T: Encodable + Decodable + Debug + 'static,
{
    fn compare(data1: &[u8], data2: &[u8]) -> std::cmp::Ordering {
        data1.cmp(data2)
    }
}

// ─── Consensus<T>: redb::Value adapter over Encodable/Decodable ──────────
//
// Bridge wrapper: uses the existing consensus-encoding path so types that
// haven't been ported to borsh yet can still live in a `NativeTableDef`.
// Planned to be deleted once every V type has a borsh impl.

#[derive(Debug)]
pub struct Consensus<T>(PhantomData<T>);

impl<T> redb::Value for Consensus<T>
where
    T: Encodable + Decodable + Debug + 'static,
{
    type SelfType<'a>
        = T
    where
        Self: 'a;

    type AsBytes<'a>
        = Vec<u8>
    where
        Self: 'a;

    fn fixed_width() -> Option<usize> {
        None
    }

    fn from_bytes<'a>(data: &'a [u8]) -> T
    where
        Self: 'a,
    {
        decode_with_current_scope::<T>(data)
    }

    fn as_bytes<'a, 'b: 'a>(value: &'a T) -> Vec<u8>
    where
        Self: 'b,
    {
        value.consensus_encode_to_vec()
    }

    fn type_name() -> redb::TypeName {
        redb::TypeName::new(&format!(
            "fedimint::Consensus<{}>",
            std::any::type_name::<T>()
        ))
    }
}

/// Implement `redb::Key` + `redb::Value` for a fixed-width primitive newtype.
///
/// ```ignore
/// pub struct EventLogId(pub u64);
/// redb_newtype_key!(EventLogId, u64);
/// ```
///
/// The newtype must be `struct Foo(pub $inner)`. Encoding delegates to the
/// inner primitive's redb impl, which is LE fixed-width with integer-correct
/// `compare`.
#[macro_export]
macro_rules! redb_newtype_key {
    ($ty:ty, $inner:ty) => {
        impl $crate::redb::Value for $ty {
            type SelfType<'a>
                = $ty
            where
                Self: 'a;

            type AsBytes<'a>
                = <$inner as $crate::redb::Value>::AsBytes<'a>
            where
                Self: 'a;

            fn fixed_width() -> Option<usize> {
                <$inner as $crate::redb::Value>::fixed_width()
            }

            fn from_bytes<'a>(data: &'a [u8]) -> Self
            where
                Self: 'a,
            {
                Self(<$inner as $crate::redb::Value>::from_bytes(data))
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
            where
                Self: 'b,
            {
                <$inner as $crate::redb::Value>::as_bytes(&value.0)
            }

            fn type_name() -> $crate::redb::TypeName {
                $crate::redb::TypeName::new(concat!("fedimint::", stringify!($ty)))
            }
        }

        impl $crate::redb::Key for $ty {
            fn compare(data1: &[u8], data2: &[u8]) -> ::std::cmp::Ordering {
                <$inner as $crate::redb::Key>::compare(data1, data2)
            }
        }
    };
}

// ─── NativeTableDef: redb-native typed table reference ───────────────────
//
// Parallel to `TableDef<K, V>`, but `K`/`V` are redb types
// (implement `redb::Key`/`redb::Value`). Gives direct access to redb's
// native typed `TableDefinition<K, V>` with zero bytes-level indirection —
// intended to replace `TableDef` once the migration is complete.

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
/// Expands to `NativeTableDef<ConsensusKey<K>, Consensus<V>>` by default —
/// the `ConsensusKey`/`Consensus` wrappers bridge the existing
/// Encodable/Decodable impls into redb's native typed-table path so tables
/// can migrate without touching every `K`/`V` type's derive list.
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
        pub const $name: $crate::db::NativeTableDef<
            $crate::db::ConsensusKey<$k>,
            $crate::db::Consensus<$v>,
        > = $crate::db::NativeTableDef::new($label);
    };
}
