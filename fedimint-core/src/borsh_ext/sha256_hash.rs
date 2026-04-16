use bitcoin::hashes::{Hash, sha256};

pub fn serialize<W: borsh::io::Write>(obj: &sha256::Hash, writer: &mut W) -> borsh::io::Result<()> {
    writer.write_all(obj.as_byte_array())
}

pub fn deserialize<R: borsh::io::Read>(reader: &mut R) -> borsh::io::Result<sha256::Hash> {
    let mut bytes = [0u8; 32];
    reader.read_exact(&mut bytes)?;
    Ok(sha256::Hash::from_byte_array(bytes))
}
