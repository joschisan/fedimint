//! Consensus-critical binary encoding.
//!
//! Two traits, one method each: [`Encodable::consensus_encode`] and
//! [`Decodable::consensus_decode`]. Fixed-width big-endian for integers,
//! length-prefixed (u32 BE) for `Vec` / `String` / maps. No varints, no
//! module decoder registry, no partial / whole / from-finite-reader tree,
//! no `DecodeError` — just `io::Error`.
//!
//! Callers that want to guard against malicious input size should bound
//! the reader (e.g. wrap in `std::io::Take`) or frame the data at the
//! transport layer. Collection decoders deliberately avoid
//! `Vec::with_capacity` so an attacker-controlled length prefix can't
//! trigger a giant allocation — the actual `Vec` only grows as the reader
//! yields bytes.

pub mod as_base64;
pub mod as_hex;
mod bls12_381;
pub mod btc;
mod iroh;
mod secp256k1;

use std::any::TypeId;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::io::{self, Read, Write};

use bitcoin::hashes::sha256;
use hex::ToHex as _;
pub use picomint_derive::{Decodable, Encodable};

use crate::util::SafeUrl;

/// Types that can encode themselves to a byte stream.
pub trait Encodable {
    fn consensus_encode<W: Write>(&self, writer: &mut W) -> io::Result<()>;

    /// Encode to a newly allocated `Vec<u8>`.
    fn consensus_encode_to_vec(&self) -> Vec<u8> {
        let mut bytes = Vec::new();
        self.consensus_encode(&mut bytes)
            .expect("writes to Vec can't fail");
        bytes
    }

    /// Encode and hex-encode the result.
    fn consensus_encode_to_hex(&self) -> String {
        self.consensus_encode_to_vec().encode_hex()
    }

    /// Hash the consensus encoding with `H`.
    fn consensus_hash<H>(&self) -> H
    where
        H: bitcoin::hashes::Hash,
        H::Engine: Write,
    {
        let mut engine = H::engine();
        self.consensus_encode(&mut engine)
            .expect("writes to HashEngine can't fail");
        H::from_engine(engine)
    }

    /// SHA-256 of the consensus encoding.
    fn consensus_hash_sha256(&self) -> sha256::Hash {
        self.consensus_hash()
    }
}

/// Types that can decode themselves from a byte stream.
pub trait Decodable: Sized {
    fn consensus_decode<R: Read>(reader: &mut R) -> io::Result<Self>;

    /// Decode from a byte slice, erroring if any bytes remain.
    fn consensus_decode_exact(bytes: &[u8]) -> io::Result<Self> {
        let mut reader = bytes;
        let value = Self::consensus_decode(&mut reader)?;
        if !reader.is_empty() {
            return Err(invalid_data(format!(
                "trailing bytes after decoding {}",
                std::any::type_name::<Self>()
            )));
        }
        Ok(value)
    }
}

fn invalid_data(msg: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.into())
}

// ─── References ─────────────────────────────────────────────────────────

impl<T> Encodable for &T
where
    T: Encodable + ?Sized,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        (**self).consensus_encode(w)
    }
}

impl<T> Encodable for Box<T>
where
    T: Encodable + ?Sized,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        (**self).consensus_encode(w)
    }
}

impl<T> Decodable for Box<T>
where
    T: Decodable,
{
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::new(T::consensus_decode(r)?))
    }
}

// ─── Integers (fixed-width big-endian) ──────────────────────────────────

macro_rules! impl_encode_decode_int {
    ($ty:ty) => {
        impl Encodable for $ty {
            fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
                w.write_all(&self.to_be_bytes())
            }
        }

        impl Decodable for $ty {
            fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
                let mut buf = [0u8; std::mem::size_of::<$ty>()];
                r.read_exact(&mut buf)?;
                Ok(Self::from_be_bytes(buf))
            }
        }
    };
}

impl_encode_decode_int!(u8);
impl_encode_decode_int!(u16);
impl_encode_decode_int!(u32);
impl_encode_decode_int!(u64);

// ─── bool ───────────────────────────────────────────────────────────────

impl Encodable for bool {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        u8::from(*self).consensus_encode(w)
    }
}

impl Decodable for bool {
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        match u8::consensus_decode(r)? {
            0 => Ok(false),
            1 => Ok(true),
            n => Err(invalid_data(format!("bool: expected 0/1, got {n}"))),
        }
    }
}

// ─── () ─────────────────────────────────────────────────────────────────

impl Encodable for () {
    fn consensus_encode<W: Write>(&self, _: &mut W) -> io::Result<()> {
        Ok(())
    }
}

impl Decodable for () {
    fn consensus_decode<R: Read>(_: &mut R) -> io::Result<Self> {
        Ok(())
    }
}

// ─── Tuples ─────────────────────────────────────────────────────────────

macro_rules! impl_encode_decode_tuple {
    ($($ty:ident),*) => {
        #[allow(non_snake_case)]
        impl<$($ty: Encodable),*> Encodable for ($($ty,)*) {
            fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
                let ($($ty,)*) = self;
                $($ty.consensus_encode(w)?;)*
                Ok(())
            }
        }

        #[allow(non_snake_case)]
        impl<$($ty: Decodable),*> Decodable for ($($ty,)*) {
            fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
                Ok(($({ let $ty = <$ty as Decodable>::consensus_decode(r)?; $ty },)*))
            }
        }
    };
}

impl_encode_decode_tuple!(T1, T2);
impl_encode_decode_tuple!(T1, T2, T3);
impl_encode_decode_tuple!(T1, T2, T3, T4);
impl_encode_decode_tuple!(T1, T2, T3, T4, T5);
impl_encode_decode_tuple!(T1, T2, T3, T4, T5, T6);

// ─── Option / Result ────────────────────────────────────────────────────

impl<T: Encodable> Encodable for Option<T> {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            None => 0u8.consensus_encode(w),
            Some(v) => {
                1u8.consensus_encode(w)?;
                v.consensus_encode(w)
            }
        }
    }
}

impl<T: Decodable> Decodable for Option<T> {
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        match u8::consensus_decode(r)? {
            0 => Ok(None),
            1 => Ok(Some(T::consensus_decode(r)?)),
            n => Err(invalid_data(format!("Option: expected 0/1, got {n}"))),
        }
    }
}

impl<T: Encodable, E: Encodable> Encodable for Result<T, E> {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        match self {
            Err(e) => {
                0u8.consensus_encode(w)?;
                e.consensus_encode(w)
            }
            Ok(v) => {
                1u8.consensus_encode(w)?;
                v.consensus_encode(w)
            }
        }
    }
}

impl<T: Decodable, E: Decodable> Decodable for Result<T, E> {
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        match u8::consensus_decode(r)? {
            0 => Ok(Err(E::consensus_decode(r)?)),
            1 => Ok(Ok(T::consensus_decode(r)?)),
            n => Err(invalid_data(format!("Result: expected 0/1, got {n}"))),
        }
    }
}

// ─── String / &str / Cow<str> ───────────────────────────────────────────

impl Encodable for String {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.as_bytes().consensus_encode(w)
    }
}

impl Decodable for String {
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        Self::from_utf8(Vec::<u8>::consensus_decode(r)?)
            .map_err(|e| invalid_data(format!("invalid UTF-8: {e}")))
    }
}

impl Encodable for str {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.as_bytes().consensus_encode(w)
    }
}

impl Encodable for Cow<'static, str> {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.as_ref().consensus_encode(w)
    }
}

impl Decodable for Cow<'static, str> {
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        Ok(Cow::Owned(String::consensus_decode(r)?))
    }
}

// ─── SafeUrl ────────────────────────────────────────────────────────────

impl Encodable for SafeUrl {
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_string().consensus_encode(w)
    }
}

impl Decodable for SafeUrl {
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        String::consensus_decode(r)?
            .parse()
            .map_err(|e: url::ParseError| invalid_data(format!("invalid SafeUrl: {e}")))
    }
}

// ─── Slices, Vec, [T; N] ────────────────────────────────────────────────
//
// `Vec<u8>` / `[u8; N]` / `&[u8]` take a specialized path via TypeId to
// avoid going through `u8::consensus_encode` byte-by-byte. Matters for
// large blobs (BFT messages, serialized configs).

impl<T> Encodable for [T]
where
    T: Encodable + 'static,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        u32::try_from(self.len())
            .expect("collection length exceeds u32")
            .consensus_encode(w)?;
        if TypeId::of::<T>() == TypeId::of::<u8>() {
            // SAFETY: T is u8, so this transmute is a no-op
            let bytes = unsafe { std::mem::transmute::<&[T], &[u8]>(self) };
            w.write_all(bytes)?;
        } else {
            for item in self {
                item.consensus_encode(w)?;
            }
        }
        Ok(())
    }
}

impl<T> Encodable for Vec<T>
where
    T: Encodable + 'static,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        self.as_slice().consensus_encode(w)
    }
}

impl<T> Decodable for Vec<T>
where
    T: Decodable + 'static,
{
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let len = u32::consensus_decode(r)? as usize;
        if TypeId::of::<T>() == TypeId::of::<u8>() {
            let mut bytes = vec![0u8; len];
            r.read_exact(&mut bytes)?;
            // SAFETY: T is u8, so this transmute is a no-op
            return Ok(unsafe { std::mem::transmute::<Vec<u8>, Self>(bytes) });
        }
        // Deliberately avoid `Vec::with_capacity(len)` — an attacker-controlled
        // length prefix could request a giant allocation. The vec grows
        // only as the reader yields bytes.
        let mut v = Vec::new();
        for _ in 0..len {
            v.push(T::consensus_decode(r)?);
        }
        Ok(v)
    }
}

impl<T, const N: usize> Encodable for [T; N]
where
    T: Encodable + 'static,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        if TypeId::of::<T>() == TypeId::of::<u8>() {
            // SAFETY: T is u8, so this transmute is a no-op
            let bytes = unsafe { std::mem::transmute::<&[T; N], &[u8; N]>(self) };
            w.write_all(bytes)
        } else {
            for item in self {
                item.consensus_encode(w)?;
            }
            Ok(())
        }
    }
}

impl<T, const N: usize> Decodable for [T; N]
where
    T: Decodable + Debug + Default + Copy + 'static,
{
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        if TypeId::of::<T>() == TypeId::of::<u8>() {
            let mut bytes = [0u8; N];
            r.read_exact(&mut bytes)?;
            // SAFETY: T is u8, so [T; N] and [u8; N] have identical layout.
            let ptr = std::ptr::from_ref(&bytes).cast::<[T; N]>();
            return Ok(unsafe { ptr.read() });
        }
        let mut data = [T::default(); N];
        for item in &mut data {
            *item = T::consensus_decode(r)?;
        }
        Ok(data)
    }
}

// ─── BTreeMap / BTreeSet (canonical ordering enforced on decode) ────────

impl<K, V> Encodable for BTreeMap<K, V>
where
    K: Encodable,
    V: Encodable,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        u32::try_from(self.len())
            .expect("collection length exceeds u32")
            .consensus_encode(w)?;
        for (k, v) in self {
            k.consensus_encode(w)?;
            v.consensus_encode(w)?;
        }
        Ok(())
    }
}

impl<K, V> Decodable for BTreeMap<K, V>
where
    K: Decodable + Ord,
    V: Decodable,
{
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let len = u32::consensus_decode(r)? as usize;
        let mut map = Self::new();
        for _ in 0..len {
            let k = K::consensus_decode(r)?;
            if map.last_key_value().is_some_and(|(prev, _)| k <= *prev) {
                return Err(invalid_data("BTreeMap: non-canonical key order"));
            }
            let v = V::consensus_decode(r)?;
            if map.insert(k, v).is_some() {
                return Err(invalid_data("BTreeMap: duplicate key"));
            }
        }
        Ok(map)
    }
}

impl<K> Encodable for BTreeSet<K>
where
    K: Encodable,
{
    fn consensus_encode<W: Write>(&self, w: &mut W) -> io::Result<()> {
        u32::try_from(self.len())
            .expect("collection length exceeds u32")
            .consensus_encode(w)?;
        for k in self {
            k.consensus_encode(w)?;
        }
        Ok(())
    }
}

impl<K> Decodable for BTreeSet<K>
where
    K: Decodable + Ord,
{
    fn consensus_decode<R: Read>(r: &mut R) -> io::Result<Self> {
        let len = u32::consensus_decode(r)? as usize;
        let mut set = Self::new();
        for _ in 0..len {
            let k = K::consensus_decode(r)?;
            if set.last().is_some_and(|prev| k <= *prev) {
                return Err(invalid_data("BTreeSet: non-canonical order"));
            }
            if !set.insert(k) {
                return Err(invalid_data("BTreeSet: duplicate element"));
            }
        }
        Ok(set)
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────

#[cfg(test)]
pub(crate) mod tests {
    use std::fmt::Debug;

    use super::*;

    pub(crate) fn test_roundtrip<T>(value: &T)
    where
        T: Encodable + Decodable + Eq + Debug,
    {
        let bytes = value.consensus_encode_to_vec();
        let decoded = T::consensus_decode_exact(&bytes).unwrap();
        assert_eq!(value, &decoded);
    }

    pub(crate) fn test_roundtrip_expected<T>(value: &T, expected: &[u8])
    where
        T: Encodable + Decodable + Eq + Debug,
    {
        let bytes = value.consensus_encode_to_vec();
        assert_eq!(expected, &bytes[..]);
        let decoded = T::consensus_decode_exact(&bytes).unwrap();
        assert_eq!(value, &decoded);
    }

    #[test]
    fn primitives_roundtrip() {
        test_roundtrip_expected(&0u8, &[0]);
        test_roundtrip_expected(&0xABu8, &[0xAB]);
        test_roundtrip_expected(&0x0102u16, &[0x01, 0x02]);
        test_roundtrip_expected(&0x0102_0304u32, &[0x01, 0x02, 0x03, 0x04]);
        test_roundtrip_expected(
            &0x0102_0304_0506_0708u64,
            &[0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08],
        );
        test_roundtrip_expected(&true, &[1]);
        test_roundtrip_expected(&false, &[0]);
    }

    #[test]
    fn vec_u8_roundtrip() {
        // u32 len (4 bytes BE) + bytes
        test_roundtrip_expected(&vec![1u8, 2, 3], &[0, 0, 0, 3, 1, 2, 3]);
        test_roundtrip_expected::<Vec<u8>>(&vec![], &[0, 0, 0, 0]);
    }

    #[test]
    fn vec_u16_roundtrip() {
        test_roundtrip_expected(&vec![1u16, 2, 3], &[0, 0, 0, 3, 0, 1, 0, 2, 0, 3]);
    }

    #[test]
    fn string_roundtrip() {
        test_roundtrip(&"".to_string());
        test_roundtrip(&"hello".to_string());
        test_roundtrip(&"héllo 🌍".to_string());
    }

    #[test]
    fn option_result_roundtrip() {
        test_roundtrip::<Option<u32>>(&None);
        test_roundtrip::<Option<u32>>(&Some(42));
        test_roundtrip::<Result<u32, String>>(&Ok(42));
        test_roundtrip::<Result<u32, String>>(&Err("oops".to_string()));
    }

    #[test]
    fn btreemap_roundtrip() {
        use std::collections::BTreeMap;
        test_roundtrip(&BTreeMap::from([
            ("a".to_string(), 1u32),
            ("b".to_string(), 2),
        ]));
    }

    #[test]
    fn btreemap_non_canonical_rejected() {
        // Manually craft bytes with keys out of order.
        let mut bad = Vec::new();
        2u32.consensus_encode(&mut bad).unwrap();
        "b".to_string().consensus_encode(&mut bad).unwrap();
        1u32.consensus_encode(&mut bad).unwrap();
        "a".to_string().consensus_encode(&mut bad).unwrap();
        2u32.consensus_encode(&mut bad).unwrap();
        assert!(BTreeMap::<String, u32>::consensus_decode_exact(&bad).is_err());
    }

    #[test]
    fn trailing_bytes_rejected() {
        let mut bytes = 42u32.consensus_encode_to_vec();
        bytes.push(0);
        assert!(u32::consensus_decode_exact(&bytes).is_err());
    }

    #[derive(Debug, Eq, PartialEq, Encodable, Decodable)]
    enum TestEnum {
        Foo,
        Bar(u32, String),
        Baz { baz: u8 },
    }

    #[test]
    fn derive_enum_roundtrip() {
        test_roundtrip(&TestEnum::Foo);
        test_roundtrip(&TestEnum::Bar(42, "hi".to_string()));
        test_roundtrip(&TestEnum::Baz { baz: 7 });
    }

    #[test]
    fn derive_unknown_enum_variant_rejected() {
        // Variant index 99 doesn't exist.
        let mut bytes = Vec::new();
        99u64.consensus_encode(&mut bytes).unwrap();
        assert!(TestEnum::consensus_decode_exact(&bytes).is_err());
    }

    #[derive(Debug, Encodable, Decodable)]
    enum NotConstructable {}

    #[test]
    fn derive_empty_enum_always_errors() {
        assert!(NotConstructable::consensus_decode_exact(&[0]).is_err());
    }

    #[derive(Debug, Encodable, Decodable, Eq, PartialEq)]
    struct TestStruct {
        vec: Vec<u8>,
        num: u32,
    }

    #[test]
    fn derive_struct_roundtrip() {
        test_roundtrip(&TestStruct {
            vec: vec![1, 2, 3],
            num: 42,
        });
    }

    #[derive(Debug, Encodable, Decodable, Eq, PartialEq)]
    struct TestTupleStruct(Vec<u8>, u32);

    #[test]
    fn derive_tuple_struct_roundtrip() {
        test_roundtrip(&TestTupleStruct(vec![1, 2, 3], 42));
    }

    #[derive(Debug, PartialEq, Eq, Encodable, Decodable)]
    enum IndexedEnum {
        #[encodable(index = 0)]
        Foo,
        #[encodable(index = 2)]
        Baz,
    }

    #[test]
    fn derive_custom_index_enum_roundtrip() {
        test_roundtrip(&IndexedEnum::Foo);
        test_roundtrip(&IndexedEnum::Baz);
    }

    fn encode_value<T: Encodable>(v: &T) -> Vec<u8> {
        v.consensus_encode_to_vec()
    }

    fn decode_value<T: Decodable>(bytes: &[u8]) -> T {
        T::consensus_decode_exact(bytes).unwrap()
    }

    fn preserves_numeric_order<T>(mut values: Vec<T>)
    where
        T: Ord + Encodable + Decodable + Debug,
    {
        values.sort();
        let mut encoded = values.iter().map(encode_value).collect::<Vec<_>>();
        encoded.sort();
        let decoded = encoded
            .iter()
            .map(|v| decode_value::<T>(v))
            .collect::<Vec<_>>();
        for (i, (a, b)) in values.iter().zip(decoded.iter()).enumerate() {
            assert_eq!(a, b, "mismatch at index {i}");
        }
    }

    #[test]
    fn bytewise_order_matches_numeric_order() {
        #[derive(Ord, PartialOrd, Eq, PartialEq, Debug, Encodable, Decodable)]
        struct Amount(u64);

        #[derive(Ord, PartialOrd, Eq, PartialEq, Debug, Encodable, Decodable)]
        struct Complex(u16, u32, u64);

        #[derive(Ord, PartialOrd, Eq, PartialEq, Debug, Encodable, Decodable)]
        struct Text(String);

        preserves_numeric_order((0..20_000).map(Amount).collect());
        preserves_numeric_order(
            (10..200)
                .flat_map(|i| {
                    (i - 1..=i + 1).flat_map(move |j| {
                        (i - 1..=i + 1).map(move |k| Complex(i as u16, j as u32, k as u64))
                    })
                })
                .collect(),
        );
        preserves_numeric_order(
            (' '..'~')
                .flat_map(|i| {
                    (' '..'~')
                        .map(|j| Text(format!("{i}{j}")))
                        .collect::<Vec<_>>()
                })
                .collect(),
        );
    }
}
