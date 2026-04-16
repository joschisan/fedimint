use bls12_381::G1Affine;

pub fn serialize<W: borsh::io::Write>(obj: &G1Affine, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(&obj.to_compressed())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<G1Affine> {
    let mut bytes = [0u8; 48];
    reader.read_exact(&mut bytes)?;
    Option::from(G1Affine::from_compressed(&bytes)).ok_or_else(|| {
        borsh::io::Error::new(
            borsh::io::ErrorKind::InvalidData,
            "invalid bls12_381::G1Affine encoding",
        )
    })
}
