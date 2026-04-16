use secp256k1::schnorr::Signature;

pub fn serialize<W: borsh::io::Write>(obj: &Signature, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(obj.as_ref())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<Signature> {
    let mut bytes = [0u8; secp256k1::constants::SCHNORR_SIGNATURE_SIZE];
    reader.read_exact(&mut bytes)?;
    Signature::from_slice(&bytes)
        .map_err(|e| borsh::io::Error::new(borsh::io::ErrorKind::InvalidData, e.to_string()))
}
