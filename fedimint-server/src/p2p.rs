//! P2P message types exchanged between federation peers over the broadcast /
//! DKG channels. Previously lived in `fedimint-core::config`; moved here with
//! the module-system rip since they are server-side only.

use bitcoin::hashes::sha256;
use bls12_381::{G1Projective, G2Projective, Scalar};
use fedimint_api_client::session_outcome::SignedSessionOutcome;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1;

#[derive(Debug, Clone, PartialEq, Eq, Encodable, Decodable)]
pub enum P2PMessage {
    Aleph(Vec<u8>),
    SessionSignature(secp256k1::schnorr::Signature),
    SessionIndex(u64),
    SignedSessionOutcome(SignedSessionOutcome),
    Checksum(sha256::Hash),
    DkgG1(DkgMessageG1),
    DkgG2(DkgMessageG2),
    Encodable(Vec<u8>),
    #[encodable_default]
    Default {
        variant: u64,
        bytes: Vec<u8>,
    },
}

#[derive(Debug, PartialEq, Eq, Clone, Encodable, Decodable)]
pub enum DkgMessageG1 {
    Hash(sha256::Hash),
    Commitment(Vec<G1Projective>),
    Share(Scalar),
}

#[derive(Debug, PartialEq, Eq, Clone, Encodable, Decodable)]
pub enum DkgMessageG2 {
    Hash(sha256::Hash),
    Commitment(Vec<G2Projective>),
    Share(Scalar),
}
