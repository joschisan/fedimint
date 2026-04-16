use secp256k1::XOnlyPublicKey;

pub fn serialize<W: borsh::io::Write>(
    obj: &XOnlyPublicKey,
    writer: &mut W,
) -> borsh::io::Result<()> {
    writer.write_all(&obj.serialize())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<XOnlyPublicKey> {
    let mut bytes = [0u8; 32];
    reader.read_exact(&mut bytes)?;
    XOnlyPublicKey::from_slice(&bytes)
        .map_err(|e| borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, e.to_string()))
}
