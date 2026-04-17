use std::io;

use bls12_381::{G1Affine, G1Projective, G2Affine, G2Projective, Scalar};
use group::Curve as _;

use super::{Decodable, Encodable};

impl Encodable for Scalar {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_bytes().consensus_encode(w)
    }
}

impl Decodable for Scalar {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let bytes = <[u8; 32]>::consensus_decode(r)?;
        Option::from(Self::from_bytes(&bytes))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid bls Scalar"))
    }
}

impl Encodable for G1Affine {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_compressed().consensus_encode(w)
    }
}

impl Decodable for G1Affine {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let bytes = <[u8; 48]>::consensus_decode(r)?;
        Option::from(Self::from_compressed(&bytes))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid bls G1Affine"))
    }
}

impl Encodable for G2Affine {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_compressed().consensus_encode(w)
    }
}

impl Decodable for G2Affine {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let bytes = <[u8; 96]>::consensus_decode(r)?;
        Option::from(Self::from_compressed(&bytes))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid bls G2Affine"))
    }
}

impl Encodable for G1Projective {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_affine().consensus_encode(w)
    }
}

impl Decodable for G1Projective {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::from(G1Affine::consensus_decode(r)?))
    }
}

impl Encodable for G2Projective {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_affine().consensus_encode(w)
    }
}

impl Decodable for G2Projective {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::from(G2Affine::consensus_decode(r)?))
    }
}
