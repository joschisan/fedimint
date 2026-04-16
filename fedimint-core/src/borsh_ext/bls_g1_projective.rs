use bls12_381::G1Projective;
use group::Curve;

pub fn serialize<W: borsh::io::Write>(
    obj: &G1Projective,
    writer: &mut W,
) -> borsh::io::Result<()> {
    super::bls_g1_affine::serialize(&obj.to_affine(), writer)
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<G1Projective> {
    super::bls_g1_affine::deserialize(reader).map(G1Projective::from)
}
