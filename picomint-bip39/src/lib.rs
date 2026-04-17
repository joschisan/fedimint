#![deny(clippy::pedantic)]

//! BIP39 client secret support crate

use std::io::{Read, Write};

pub use bip39::{Language, Mnemonic};
use picomint_client::secret::RootSecretStrategy;
use picomint_encoding::{Decodable, Encodable};
use picomint_derive_secret::DerivableSecret;
use rand::{CryptoRng, RngCore};

/// BIP39 root secret encoding strategy allowing retrieval of the seed phrase.
#[derive(Debug)]
pub struct Bip39RootSecretStrategy<const WORD_COUNT: usize = 12>;

impl<const WORD_COUNT: usize> RootSecretStrategy for Bip39RootSecretStrategy<WORD_COUNT> {
    type Encoding = Mnemonic;

    fn to_root_secret(secret: &Self::Encoding) -> DerivableSecret {
        const PICOMINT_CLIENT_NONCE: &[u8] = b"Picomint Client Salt";
        const EMPTY_PASSPHRASE: &str = "";

        DerivableSecret::new_root(
            secret.to_seed_normalized(EMPTY_PASSPHRASE).as_ref(),
            PICOMINT_CLIENT_NONCE,
        )
    }

    fn consensus_encode(secret: &Self::Encoding, writer: &mut impl Write) -> std::io::Result<()> {
        secret.to_entropy().consensus_encode(writer)
    }

    fn consensus_decode(reader: &mut impl Read) -> std::io::Result<Self::Encoding> {
        let bytes = Vec::<u8>::consensus_decode(reader)?;
        Mnemonic::from_entropy(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))
    }

    fn random<R>(rng: &mut R) -> Self::Encoding
    where
        R: RngCore + CryptoRng,
    {
        Mnemonic::generate_in_with(rng, Language::English, WORD_COUNT)
            .expect("Failed to generate mnemonic, bad word count")
    }
}
