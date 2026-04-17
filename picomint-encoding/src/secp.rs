use std::io;

use super::{Decodable, Encodable};

fn invalid(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

impl Encodable for secp256k1::ecdsa::Signature {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(&self.serialize_compact())
    }
}

impl Decodable for secp256k1::ecdsa::Signature {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_compact(&<[u8; 64]>::consensus_decode(r)?).map_err(invalid)
    }
}

impl Encodable for secp256k1::PublicKey {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.serialize().consensus_encode(w)
    }
}

impl Decodable for secp256k1::PublicKey {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_slice(&<[u8; 33]>::consensus_decode(r)?).map_err(invalid)
    }
}

impl Encodable for secp256k1::XOnlyPublicKey {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.serialize().consensus_encode(w)
    }
}

impl Decodable for secp256k1::XOnlyPublicKey {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_slice(&<[u8; 32]>::consensus_decode(r)?).map_err(invalid)
    }
}

impl Encodable for secp256k1::schnorr::Signature {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        w.write_all(self.as_ref())
    }
}

impl Decodable for secp256k1::schnorr::Signature {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let bytes = <[u8; secp256k1::constants::SCHNORR_SIGNATURE_SIZE]>::consensus_decode(r)?;
        Self::from_slice(&bytes).map_err(invalid)
    }
}

impl Encodable for secp256k1::SecretKey {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.secret_bytes().consensus_encode(w)
    }
}

impl Decodable for secp256k1::SecretKey {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_slice(&<[u8; 32]>::consensus_decode(r)?).map_err(invalid)
    }
}

impl Encodable for bitcoin::key::Keypair {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.secret_bytes().consensus_encode(w)
    }
}

impl Decodable for bitcoin::key::Keypair {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let bytes = <[u8; 32]>::consensus_decode(r)?;
        Self::from_seckey_slice(bitcoin::secp256k1::global::SECP256K1, &bytes).map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use secp256k1::Message;
    use secp256k1::hashes::Hash as BitcoinHash;

    use super::super::tests::test_roundtrip;

    #[test]
    fn ecdsa_sig_roundtrip() {
        let ctx = secp256k1::Secp256k1::new();
        let (sk, _) = ctx.generate_keypair(&mut rand::thread_rng());
        let sig = ctx.sign_ecdsa(
            &Message::from_digest(*secp256k1::hashes::sha256::Hash::hash(b"Hello!").as_ref()),
            &sk,
        );
        test_roundtrip(&sig);
    }

    #[test]
    fn schnorr_roundtrip() {
        let ctx = secp256k1::global::SECP256K1;
        let kp = bitcoin::key::Keypair::new(ctx, &mut rand::rngs::OsRng);
        test_roundtrip(&kp.public_key());
        let sig = ctx.sign_schnorr(
            &Message::from_digest(*secp256k1::hashes::sha256::Hash::hash(b"Hello!").as_ref()),
            &kp,
        );
        test_roundtrip(&sig);
    }
}
