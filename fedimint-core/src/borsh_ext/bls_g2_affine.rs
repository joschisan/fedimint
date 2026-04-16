use bls12_381::G2Affine;

pub fn serialize<W: borsh::io::Write>(obj: &G2Affine, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(&obj.to_compressed())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<G2Affine> {
    let mut bytes = [0u8; 96];
    reader.read_exact(&mut bytes)?;
    Option::from(G2Affine::from_compressed(&bytes)).ok_or_else(|| {
        borsh::io::Error::new(
            borsh::io::ErrorKind::InvalidData,
            "invalid bls12_381::G2Affine encoding",
        )
    })
}
