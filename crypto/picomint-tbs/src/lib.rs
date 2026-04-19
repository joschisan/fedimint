//! # Threshold Blind Signatures
//!
//! This library implements an ad-hoc threshold blind signature scheme based on
//! BLS signatures using the (unrelated) BLS12-381 curve.

use bitcoin::hashes::Hash as BitcoinHash;
use bitcoin::hashes::sha256;
use bls12_381::{G1Affine, G1Projective, G2Affine, G2Projective, Scalar, pairing};
use group::{Curve, Group};
use picomint_encoding::{Decodable, Encodable};
use rand::SeedableRng;
use std::collections::BTreeMap;
mod bls_serde;
use rand_chacha::ChaChaRng;
use serde::{Deserialize, Serialize};

const TAG: [u8; 30] = *b"PICOMINT_TBS_BLS12_381_MESSAGE";

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct SecretKeyShare(#[serde(with = "bls_serde::scalar")] pub Scalar);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct PublicKeyShare(#[serde(with = "bls_serde::g2")] pub G2Affine);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct AggregatePublicKey(#[serde(with = "bls_serde::g2")] pub G2Affine);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct Message(#[serde(with = "bls_serde::g1")] pub G1Affine);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct BlindingKey(#[serde(with = "bls_serde::scalar")] pub Scalar);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct BlindedMessage(#[serde(with = "bls_serde::g1")] pub G1Affine);

picomint_redb::consensus_key!(BlindedMessage);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct BlindedSignatureShare(#[serde(with = "bls_serde::g1")] pub G1Affine);

picomint_redb::consensus_value!(BlindedSignatureShare);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct BlindedSignature(#[serde(with = "bls_serde::g1")] pub G1Affine);

#[derive(Copy, Clone, Debug, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct Signature(#[serde(with = "bls_serde::g1")] pub G1Affine);

macro_rules! point_hash_impl {
    ($type:ty) => {
        impl std::hash::Hash for $type {
            fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
                self.0.to_compressed().hash(state);
            }
        }
    };
}

point_hash_impl!(PublicKeyShare);
point_hash_impl!(AggregatePublicKey);
point_hash_impl!(Message);
point_hash_impl!(BlindedMessage);
point_hash_impl!(BlindedSignatureShare);
point_hash_impl!(BlindedSignature);
point_hash_impl!(Signature);

pub fn derive_pk_share(sk: &SecretKeyShare) -> PublicKeyShare {
    PublicKeyShare((G2Projective::generator() * sk.0).to_affine())
}

impl std::hash::Hash for BlindingKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bytes().hash(state);
    }
}

impl Message {
    /// Creates a [`Message`] by hashing a 33-byte compressed public key with
    /// SHA-256 under the domain separator `PICOMINT_TBS_BLS12_381_MESSAGE`,
    /// then mapping the hash to a BLS12-381 G1 curve point via a seeded
    /// [`ChaChaRng`].
    pub fn from_public_key(bytes: [u8; 33]) -> Message {
        let seed = (TAG, bytes)
            .consensus_hash::<sha256::Hash>()
            .to_byte_array();

        Message(G1Projective::random(&mut ChaChaRng::from_seed(seed)).to_affine())
    }
}

pub fn blind_message(msg: Message, blinding_key: BlindingKey) -> BlindedMessage {
    let blinded_msg = msg.0 * blinding_key.0;

    BlindedMessage(blinded_msg.to_affine())
}

pub fn sign_message(msg: BlindedMessage, sks: SecretKeyShare) -> BlindedSignatureShare {
    let sig = msg.0 * sks.0;
    BlindedSignatureShare(sig.to_affine())
}

pub fn verify_signature_share(
    msg: BlindedMessage,
    sig: BlindedSignatureShare,
    pk: PublicKeyShare,
) -> bool {
    pairing(&msg.0, &pk.0) == pairing(&sig.0, &G2Affine::generator())
}

/// Combines the exact threshold of valid blinded signature shares to a blinded
/// signature. The responsibility of verifying the shares and supplying
/// exactly the necessary threshold of shares lies with the caller.
/// # Panics
/// If shares is empty
pub fn aggregate_signature_shares(
    shares: &BTreeMap<u64, BlindedSignatureShare>,
) -> BlindedSignature {
    // this is a special case for one-of-one federations
    if shares.len() == 1 {
        return BlindedSignature(
            shares
                .values()
                .next()
                .expect("We have at least one value")
                .0,
        );
    }

    BlindedSignature(
        lagrange_multipliers(
            shares
                .keys()
                .cloned()
                .map(|peer| Scalar::from(peer + 1))
                .collect(),
        )
        .into_iter()
        .zip(shares.values())
        .map(|(lagrange_multiplier, share)| lagrange_multiplier * share.0)
        .reduce(|a, b| a + b)
        .expect("We have at least one share")
        .to_affine(),
    )
}

// TODO: aggregating public key shares is hacky since we can obtain the
// aggregated public by evaluating the dkg polynomial at zero - this function
// should be removed, however it is currently needed in the mint module to
// until we add the aggregated public key to the mint config.
pub fn aggregate_public_key_shares(shares: &BTreeMap<u64, PublicKeyShare>) -> AggregatePublicKey {
    // this is a special case for one-of-one federations
    if shares.len() == 1 {
        return AggregatePublicKey(
            shares
                .values()
                .next()
                .expect("We have at least one value")
                .0,
        );
    }

    AggregatePublicKey(
        lagrange_multipliers(
            shares
                .keys()
                .cloned()
                .map(|peer| Scalar::from(peer + 1))
                .collect(),
        )
        .into_iter()
        .zip(shares.values())
        .map(|(lagrange_multiplier, share)| lagrange_multiplier * share.0)
        .reduce(|a, b| a + b)
        .expect("We have at least one share")
        .to_affine(),
    )
}

fn lagrange_multipliers(scalars: Vec<Scalar>) -> Vec<Scalar> {
    scalars
        .iter()
        .map(|i| {
            scalars
                .iter()
                .filter(|j| *j != i)
                .map(|j| j * (j - i).invert().expect("We filtered the case j == i"))
                .reduce(|a, b| a * b)
                .expect("We have at least one share")
        })
        .collect()
}

pub fn verify_blinded_signature(
    msg: BlindedMessage,
    sig: BlindedSignature,
    pk: AggregatePublicKey,
) -> bool {
    pairing(&msg.0, &pk.0) == pairing(&sig.0, &G2Affine::generator())
}

pub fn unblind_signature(blinding_key: BlindingKey, blinded_sig: BlindedSignature) -> Signature {
    let sig = blinded_sig.0 * blinding_key.0.invert().unwrap();
    Signature(sig.to_affine())
}

pub fn verify(msg: Message, sig: Signature, pk: AggregatePublicKey) -> bool {
    pairing(&msg.0, &pk.0) == pairing(&sig.0, &G2Affine::generator())
}

#[cfg(test)]
mod tests;
