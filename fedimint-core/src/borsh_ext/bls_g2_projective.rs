use bls12_381::G2Projective;
use group::Curve;

pub fn serialize<W: borsh::io::Write>(
    obj: &G2Projective,
    writer: &mut W,
) -> borsh::io::Result<()> {
    super::bls_g2_affine::serialize(&obj.to_affine(), writer)
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<G2Projective> {
    super::bls_g2_affine::deserialize(reader).map(G2Projective::from)
}
