use std::io;
use std::str::FromStr;

use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::Hash as BitcoinHash;
use miniscript::{Descriptor, MiniscriptKey};

use super::{Decodable, Encodable};
use crate::get_network_for_address;

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

impl_bitcoin_bridge!(bitcoin::block::Header);
impl_bitcoin_bridge!(bitcoin::BlockHash);
impl_bitcoin_bridge!(bitcoin::OutPoint);
impl_bitcoin_bridge!(bitcoin::TxOut);
impl_bitcoin_bridge!(bitcoin::ScriptBuf);
impl_bitcoin_bridge!(bitcoin::Transaction);
impl_bitcoin_bridge!(bitcoin::merkle_tree::PartialMerkleTree);
impl_bitcoin_bridge!(bitcoin::Txid);

impl Encodable for bitcoin::psbt::Psbt {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.serialize_to_writer(bitcoin_io::from_std_mut(w))
            .map(|_| ())
            .map_err(invalid)
    }
}

impl Decodable for bitcoin::psbt::Psbt {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        // Psbt::deserialize_from_reader needs `bitcoin_io::BufRead`; a plain
        // reader is bridged through `std::io::BufReader` first.
        let mut buffered = io::BufReader::new(r);
        Self::deserialize_from_reader(bitcoin_io::from_std_mut(&mut buffered)).map_err(invalid)
    }
}

impl<K> Encodable for Descriptor<K>
where
    K: MiniscriptKey,
{
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.to_string().consensus_encode(w)
    }
}

impl<K> Decodable for Descriptor<K>
where
    Self: FromStr,
    <Self as FromStr>::Err: std::fmt::Display,
    K: MiniscriptKey,
{
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        Self::from_str(&String::consensus_decode(r)?).map_err(invalid)
    }
}

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

impl Encodable for bitcoin::Address<NetworkUnchecked> {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        get_network_for_address(self).consensus_encode(w)?;
        self.clone()
            .assume_checked()
            .script_pubkey()
            .consensus_encode(w)
    }
}

impl Decodable for bitcoin::Address<NetworkUnchecked> {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let network = bitcoin::Network::consensus_decode(r)?;
        let script = bitcoin::ScriptBuf::consensus_decode(r)?;
        bitcoin::Address::from_script(&script, network)
            .map(bitcoin::Address::into_unchecked)
            .map_err(invalid)
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

impl Encodable for lightning_invoice::RoutingFees {
    fn consensus_encode<W: io::Write>(&self, w: &mut W) -> io::Result<()> {
        self.base_msat.consensus_encode(w)?;
        self.proportional_millionths.consensus_encode(w)
    }
}

impl Decodable for lightning_invoice::RoutingFees {
    fn consensus_decode<R: io::Read>(r: &mut R) -> io::Result<Self> {
        let base_msat = u32::consensus_decode(r)?;
        let proportional_millionths = u32::consensus_decode(r)?;
        Ok(Self {
            base_msat,
            proportional_millionths,
        })
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
        let tx = bitcoin::Transaction::consensus_decode_whole(&raw).unwrap();
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
    fn address_roundtrip() {
        for s in [
            "bc1p2wsldez5mud2yam29q22wgfh9439spgduvct83k3pm50fcxa5dps59h4z5",
            "mxMYaq5yWinZ9AKjCDcBEbiEwPJD9n2uLU",
            "1FK8o7mUxyd6QWJAUw7J4vW7eRxuyjj6Ne",
            "3JSrSU7z7R1Yhh26pt1zzRjQz44qjcrXwb",
            "tb1qunn0thpt8uk3yk2938ypjccn3urxprt78z9ccq",
            "2MvUMRv2DRHZi3VshkP7RMEU84mVTfR9xjq",
        ] {
            let addr = bitcoin::Address::from_str(s).unwrap();
            let encoded = addr.consensus_encode_to_vec();
            let decoded = bitcoin::Address::consensus_decode_whole(&encoded).unwrap();
            assert_eq!(addr, decoded);
        }
    }

    #[test]
    fn sha256_roundtrip() {
        test_roundtrip(&bitcoin::hashes::sha256::Hash::hash(b"Hello world!"));
    }
}
