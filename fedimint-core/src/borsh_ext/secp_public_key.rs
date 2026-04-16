use secp256k1::PublicKey;

pub fn serialize<W: borsh::io::Write>(obj: &PublicKey, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(&obj.serialize())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<PublicKey> {
    let mut bytes = [0u8; 33];
    reader.read_exact(&mut bytes)?;
    PublicKey::from_slice(&bytes)
        .map_err(|e| borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, e.to_string()))
}
