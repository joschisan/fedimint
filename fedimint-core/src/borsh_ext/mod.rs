//! Borsh `serialize_with` / `deserialize_with` adapters for foreign types
//! we can't `impl BorshSerialize`/`BorshDeserialize` on directly.
//!
//! Each submodule exposes a `serialize` / `deserialize` pair that matches
//! borsh's field-attribute contract:
//!
//! ```ignore
//! use borsh::{BorshSerialize, BorshDeserialize};
//! use bls12_381::Scalar;
//!
//! #[derive(BorshSerialize, BorshDeserialize)]
//! struct Share {
//!     #[borsh(
//!         serialize_with = "fedimint_core::borsh_ext::bls_scalar::serialize",
//!         deserialize_with = "fedimint_core::borsh_ext::bls_scalar::deserialize",
//!     )]
//!     s: Scalar,
//! }
//! ```

pub mod bls_g1_affine;
pub mod bls_g1_projective;
pub mod bls_g2_affine;
pub mod bls_g2_projective;
pub mod bls_scalar;
pub mod secp_ecdsa_signature;
pub mod secp_public_key;
pub mod secp_schnorr_signature;
pub mod secp_secret_key;
pub mod secp_xonly_public_key;
pub mod sha256_hash;

#[cfg(test)]
mod tests;
