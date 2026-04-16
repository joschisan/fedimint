//! Serde adapter that encodes an `Encodable` as a hex string.
//!
//! Field use:
//! ```ignore
//! #[serde(with = "::picomint_core::encoding::as_hex")]
//! ```
//!
//! Whole-type use: [`crate::serde_as_encodable_hex`].

use serde::Deserialize;

use super::{Decodable, Encodable};

pub fn serialize<T, S>(t: &T, ser: S) -> Result<S::Ok, S::Error>
where
    T: Encodable,
    S: serde::Serializer,
{
    ser.serialize_str(&t.consensus_encode_to_hex())
}

pub fn deserialize<'de, T: Decodable, D>(de: D) -> Result<T, D::Error>
where
    D: serde::de::Deserializer<'de>,
{
    let hex = String::deserialize(de)?;
    let bytes = hex::decode(&hex).map_err(serde::de::Error::custom)?;
    T::consensus_decode_exact(&bytes).map_err(serde::de::Error::custom)
}

#[macro_export]
macro_rules! serialize_as_encodable_hex {
    ($name:ident) => {
        impl Serialize for $name {
            fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
            where
                S: serde::Serializer,
            {
                use $crate::Encodable;
                serializer.serialize_str(&self.consensus_encode_to_hex())
            }
        }
    };
}

#[macro_export]
macro_rules! deserialize_as_encodable_hex {
    ($name:ident) => {
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let hex = String::deserialize(deserializer)?;
                let bytes = ::picomint_core::hex::decode(&hex).map_err(serde::de::Error::custom)?;
                <Self as $crate::Decodable>::consensus_decode_exact(&bytes)
                    .map_err(serde::de::Error::custom)
            }
        }
    };
}

#[macro_export]
macro_rules! serde_as_encodable_hex {
    ($name:ident) => {
        $crate::serialize_as_encodable_hex!($name);
        $crate::deserialize_as_encodable_hex!($name);
    };
}
