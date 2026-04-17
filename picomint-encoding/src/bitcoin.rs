use std::io;

use bitcoin::hashes::Hash as BitcoinHash;

use super::{Decodable, Encodable};

fn invalid(msg: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg.to_string())
}

macro_rules! impl_bitcoin_bridge {
    ($ty:ty) => {
        impl Encodable for $ty {
            fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
                bitcoin::consensus::Encodable::consensus_encode(self, bitcoin_io::from_std_mut(w))
                    .map(|_| ())
                    .map_err(invalid)
            }
        }

        impl Decodable for $ty {
            fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
                bitcoin::consensus::Decodable::consensus_decode(bitcoin_io::from_std_mut(r))
                    .map_err(invalid)
            }
        }
    };
}

impl_bitcoin_bridge!(bitcoin::BlockHash);
impl_bitcoin_bridge!(bitcoin::OutPoint);
impl_bitcoin_bridge!(bitcoin::TxOut);
impl_bitcoin_bridge!(bitcoin::ScriptBuf);
impl_bitcoin_bridge!(bitcoin::Transaction);
impl_bitcoin_bridge!(bitcoin::Txid);

impl Encodable for bitcoin::Network {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.magic().to_bytes().consensus_encode(w)
    }
}

impl Decodable for bitcoin::Network {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_magic(bitcoin::p2p::Magic::from_bytes(
            <[u8; 4]>::consensus_decode(r)?,
        ))
        .ok_or_else(|| invalid("unknown network magic"))
    }
}

impl Encodable for bitcoin::Amount {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_sat().consensus_encode(w)
    }
}

impl Decodable for bitcoin::Amount {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::from_sat(u64::consensus_decode(r)?))
    }
}

impl Encodable for bitcoin::hashes::sha256::Hash {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_byte_array().consensus_encode(w)
    }
}

impl Decodable for bitcoin::hashes::sha256::Hash {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::from_byte_array(<[u8; 32]>::consensus_decode(r)?))
    }
}

impl Encodable for bitcoin::hashes::hash160::Hash {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_byte_array().consensus_encode(w)
    }
}

impl Decodable for bitcoin::hashes::hash160::Hash {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Ok(Self::from_byte_array(<[u8; 20]>::consensus_decode(r)?))
    }
}

impl Encodable for lightning_invoice::Bolt11Invoice {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_string().consensus_encode(w)
    }
}

impl Decodable for lightning_invoice::Bolt11Invoice {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        String::consensus_decode(r)?.parse().map_err(invalid)
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use bitcoin::hashes::Hash as BitcoinHash;
    use hex::FromHex;

    use super::super::tests::test_roundtrip;
    use super::*;

    #[test]
    fn block_hash_roundtrip() {
        let h = bitcoin::BlockHash::from_str(
            "0000000000000000000065bda8f8a88f2e1e00d9a6887a43d640e52a4c7660f2",
        )
        .unwrap();
        test_roundtrip(&h);
    }

    #[test]
    fn tx_roundtrip() {
        let raw: Vec<u8> = FromHex::from_hex(
            "02000000000101d35b66c54cf6c09b81a8d94cd5d179719cd7595c258449452a9305ab9b12df250200000000fdffffff020cd50a0000000000160014ae5d450b71c04218e6e81c86fcc225882d7b7caae695b22100000000160014f60834ef165253c571b11ce9fa74e46692fc5ec10248304502210092062c609f4c8dc74cd7d4596ecedc1093140d90b3fd94b4bdd9ad3e102ce3bc02206bb5a6afc68d583d77d5d9bcfb6252a364d11a307f3418be1af9f47f7b1b3d780121026e5628506ecd33242e5ceb5fdafe4d3066b5c0f159b3c05a621ef65f177ea28600000000"
        ).unwrap();
        let tx = bitcoin::Transaction::consensus_decode_exact(&raw).unwrap();
        test_roundtrip(&tx);
    }

    #[test]
    fn txid_roundtrip() {
        let txid = bitcoin::Txid::from_str(
            "51f7ed2f23e58cc6e139e715e9ce304a1e858416edc9079dd7b74fa8d2efc09a",
        )
        .unwrap();
        test_roundtrip(&txid);
    }

    #[test]
    fn network_roundtrip() {
        for net in [
            bitcoin::Network::Bitcoin,
            bitcoin::Network::Testnet,
            bitcoin::Network::Testnet4,
            bitcoin::Network::Signet,
            bitcoin::Network::Regtest,
        ] {
            test_roundtrip(&net);
        }
    }

    #[test]
    fn sha256_roundtrip() {
        test_roundtrip(&bitcoin::hashes::sha256::Hash::hash(b"Hello world!"));
    }
}
