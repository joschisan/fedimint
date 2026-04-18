//! BLS polynomial helpers used by DKG for both G1 and G2.

use bls12_381::{G1Affine, G1Projective, G2Affine, G2Projective, Scalar};
use group::Curve;
use picomint_core::PeerId;

pub fn g1(scalar: &Scalar) -> G1Projective {
    G1Projective::generator() * scalar
}

pub fn g2(scalar: &Scalar) -> G2Projective {
    G2Projective::generator() * scalar
}

// Offset by 1, since evaluating a poly at 0 reveals the secret
pub fn scalar(peer: &PeerId) -> Scalar {
    Scalar::from(peer.to_usize() as u64 + 1)
}

pub fn eval_poly_g1(coefficients: &[G1Projective], peer: &PeerId) -> G1Affine {
    coefficients
        .iter()
        .copied()
        .rev()
        .reduce(|acc, coefficient| acc * scalar(peer) + coefficient)
        .expect("We have at least one coefficient")
        .to_affine()
}

pub fn eval_poly_g2(coefficients: &[G2Projective], peer: &PeerId) -> G2Affine {
    coefficients
        .iter()
        .copied()
        .rev()
        .reduce(|acc, coefficient| acc * scalar(peer) + coefficient)
        .expect("We have at least one coefficient")
        .to_affine()
}
