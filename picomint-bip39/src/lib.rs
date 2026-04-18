#![deny(clippy::pedantic)]

//! BIP39 client secret support crate

pub use bip39::{Language, Mnemonic};
use picomint_derive_secret::DerivableSecret;
use rand::{CryptoRng, RngCore};

const PICOMINT_CLIENT_NONCE: &[u8] = b"Picomint Client Salt";
const EMPTY_PASSPHRASE: &str = "";
const WORD_COUNT: usize = 12;

/// Derive a picomint client root secret from a BIP39 mnemonic.
pub fn to_root_secret(mnemonic: &Mnemonic) -> DerivableSecret {
    DerivableSecret::new_root(
        mnemonic.to_seed_normalized(EMPTY_PASSPHRASE).as_ref(),
        PICOMINT_CLIENT_NONCE,
    )
}

/// Generate a fresh 12-word English BIP39 mnemonic.
pub fn random<R: RngCore + CryptoRng>(rng: &mut R) -> Mnemonic {
    Mnemonic::generate_in_with(rng, Language::English, WORD_COUNT)
        .expect("Failed to generate mnemonic, bad word count")
}
