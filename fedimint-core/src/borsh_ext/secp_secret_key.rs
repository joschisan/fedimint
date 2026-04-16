use secp256k1::SecretKey;

pub fn serialize<W: borsh::io::Write>(obj: &SecretKey, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(&obj.secret_bytes())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<SecretKey> {
    let mut bytes = [0u8; 32];
    reader.read_exact(&mut bytes)?;
    SecretKey::from_slice(&bytes)
        .map_err(|e| borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, e.to_string()))
}
