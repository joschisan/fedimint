use std::io;

use super::{Decodable, Encodable};

impl Encodable for iroh_base::SecretKey {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_bytes().consensus_encode(w)
    }
}

impl Decodable for iroh_base::SecretKey {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::from_bytes(&<[u8; 32]>::consensus_decode(r)?))
    }
}

impl Encodable for iroh_base::PublicKey {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.as_bytes().consensus_encode(w)
    }
}

impl Decodable for iroh_base::PublicKey {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_bytes(&<[u8; 32]>::consensus_decode(r)?)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
    }
}
