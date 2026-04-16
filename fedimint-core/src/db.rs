//! Abstract v2 database contract.
//!
//! A [`NativeTableDef<K, V>`] is a typed table reference backed by redb's
//! native `TableDefinition<K, V>`. Keys implement `redb::Key + redb::Value`
//! directly; values implement `redb::Value` directly. Four per-type helper
//! macros:
//!
//! - [`consensus_value!`] — implements `redb::Value` via consensus encoding.
//! - [`consensus_key!`] — implements `redb::Key` + `redb::Value` via consensus
//!   encoding with byte-lex compare.
//! - [`redb_newtype_key!`] — primitive-newtype keys (delegates to the
//!   inner primitive's redb impl for integer-correct compare).
//! - [`redb_sha256_key!`] — sha256-hash newtype keys (32 raw bytes, byte-lex
//!   compare == numeric compare).
//!
//! The concrete redb-backed tx types (`Database`, `ReadTransaction`,
//! `WriteTransaction`, `ReadTxRef`, `WriteTxRef`) live in `fedimint-redb` and
//! expose `insert`/`get`/`remove`/`iter`/`range`/`delete_table` as inherent
//! methods over `NativeTableDef`.

use std::marker::PhantomData;

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
                <$ty as $crate::encoding::Decodable>::consensus_decode_whole(data)
                    .expect("consensus_decode failed")
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self) -> ::std::vec::Vec<u8>
            where
                Self: 'b,
            {
                <$ty as $crate::encoding::Encodable>::consensus_encode_to_vec(value)
            }

            fn type_name() -> $crate::redb::TypeName {
                $crate::redb::TypeName::new(concat!("fedimint::", stringify!($ty)))
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

/// Implement `redb::Key` + `redb::Value` for a fixed-width primitive newtype.
///
/// ```ignore
/// pub struct EventLogId(pub u64);
/// redb_newtype_key!(EventLogId, u64);
/// ```
///
/// The newtype must be `struct Foo(pub $inner)`. Encoding delegates to the
/// inner primitive's redb impl, which is big-endian fixed-width with
/// integer-correct `compare`.
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

/// Implement `Encodable` + `Decodable` + `redb::Key` + `redb::Value` for a
/// 32-byte sha256 newtype. Encodes as 32 raw bytes (no length prefix); redb
/// compare is byte-lex (== numeric for fixed-width BE bytes).
///
/// ```ignore
/// pub struct FederationId(pub bitcoin::hashes::sha256::Hash);
/// redb_sha256_key!(FederationId);
/// ```
#[macro_export]
macro_rules! redb_sha256_key {
    ($ty:ty) => {
        impl $crate::encoding::Encodable for $ty {
            fn consensus_encode<W: ::std::io::Write>(
                &self,
                writer: &mut W,
            ) -> ::std::io::Result<()> {
                use $crate::bitcoin::hashes::Hash as _;
                writer.write_all(&self.0.to_byte_array())
            }
        }

        impl $crate::encoding::Decodable for $ty {
            fn consensus_decode<R: ::std::io::Read>(reader: &mut R) -> ::std::io::Result<Self> {
                use $crate::bitcoin::hashes::Hash as _;
                let mut bytes = [0u8; 32];
                reader.read_exact(&mut bytes)?;
                Ok(Self($crate::bitcoin::hashes::sha256::Hash::from_byte_array(
                    bytes,
                )))
            }
        }

        impl $crate::redb::Value for $ty {
            type SelfType<'a>
                = $ty
            where
                Self: 'a;

            type AsBytes<'a>
                = [u8; 32]
            where
                Self: 'a;

            fn fixed_width() -> Option<usize> {
                Some(32)
            }

            fn from_bytes<'a>(data: &'a [u8]) -> Self
            where
                Self: 'a,
            {
                use $crate::bitcoin::hashes::Hash as _;
                let bytes: [u8; 32] = data.try_into().expect("sha256 hash is always 32 bytes");
                Self($crate::bitcoin::hashes::sha256::Hash::from_byte_array(
                    bytes,
                ))
            }

            fn as_bytes<'a, 'b: 'a>(value: &'a Self::SelfType<'b>) -> Self::AsBytes<'a>
            where
                Self: 'b,
            {
                use $crate::bitcoin::hashes::Hash as _;
                value.0.to_byte_array()
            }

            fn type_name() -> $crate::redb::TypeName {
                $crate::redb::TypeName::new(concat!("fedimint::", stringify!($ty)))
            }
        }

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
/// directly — see `consensus_value!`, `consensus_key!`, `redb_newtype_key!`,
/// and `redb_sha256_key!`.
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
        pub const $name: $crate::db::NativeTableDef<$k, $v> =
            $crate::db::NativeTableDef::new($label);
    };
}
