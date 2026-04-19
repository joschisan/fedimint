//! Deterministic key derivation.
//!
//! `Secret` is a newtype over `sha256::Hash`. Build a root from any
//! `Encodable` seed with [`Secret::new_root`], then descend the tree by
//! passing `Encodable` path values to [`Secret::child`]. Each hop prefixes
//! with a fixed tag so the tree cannot alias any other hash output.

use bitcoin::hashes::{Hash, sha256};
use bls12_381::Scalar;
use group::ff::Field;
use rand::SeedableRng;
use rand_chacha::ChaChaRng;

use crate::secp256k1::{self, Keypair, SECP256K1};
use picomint_encoding::Encodable;

const ROOT_TAG: &[u8] = b"PICOMINT_CLIENT_SECRET_ROOT";
const CHILD_TAG: &[u8] = b"PICOMINT_CLIENT_SECRET_CHILD";

#[derive(Copy, Clone, Debug)]
pub struct Secret(sha256::Hash);

impl Secret {
    pub fn new_root<T: Encodable>(seed: &T) -> Self {
        Self((ROOT_TAG, seed).consensus_hash::<sha256::Hash>())
    }

    pub fn child<T: Encodable>(&self, path: &T) -> Self {
        Self((CHILD_TAG, path, self.0).consensus_hash::<sha256::Hash>())
    }

    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_byte_array()
    }

    pub fn to_secp_keypair(&self) -> Keypair {
        Keypair::from_seckey_slice(SECP256K1, &self.to_bytes())
            .expect("32-byte hash is within curve order")
    }

    pub fn to_secp_scalar(&self) -> secp256k1::Scalar {
        secp256k1::Scalar::from_be_bytes(self.to_bytes())
            .expect("32-byte hash is within curve order")
    }

    pub fn to_bls_scalar(&self) -> Scalar {
        Scalar::random(&mut ChaChaRng::from_seed(self.to_bytes()))
    }
}
