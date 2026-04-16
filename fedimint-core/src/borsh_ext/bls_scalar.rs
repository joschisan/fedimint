use bls12_381::Scalar;

pub fn serialize<W: borsh::io::Write>(obj: &Scalar, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(&obj.to_bytes())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Scalar> {
    let mut bytes = [0u8; 32];
    reader.read_exact(&mut bytes)?;
    Option::from(Scalar::from_bytes(&bytes)).ok_or_else(|| {
        borsh::io::Error::new(
            borsh::io::ErrorKind::InvalidData,
            "invalid bls12_381::Scalar encoding",
        )
    })
}
