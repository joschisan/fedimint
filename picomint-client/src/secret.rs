//! Mnemonic-driven client-tree derivation on top of `picomint_core::secret::Secret`.
//!
//! ```text
//! mnemonic
//!   → Secret::new_root(seed)
//!   → .child(&federation_id)            // per-federation root
//!   → .child(&ModuleKind::{Mint,Wallet,Ln})
//! ```

pub use bip39::{Language, Mnemonic};
use picomint_core::config::FederationId;
use picomint_core::core::ModuleKind;
pub use picomint_core::secret::Secret;
use rand::{CryptoRng, RngCore};

const WORD_COUNT: usize = 12;

/// Generate a fresh 12-word English BIP39 mnemonic.
pub fn random<R: RngCore + CryptoRng>(rng: &mut R) -> Mnemonic {
    Mnemonic::generate_in_with(rng, Language::English, WORD_COUNT)
        .expect("Failed to generate mnemonic, bad word count")
}

pub(crate) fn client_root(mnemonic: &Mnemonic, federation_id: FederationId) -> Secret {
    Secret::new_root(&mnemonic.to_seed_normalized("")).child(&federation_id)
}

pub(crate) fn module_secret(root: &Secret, kind: ModuleKind) -> Secret {
    root.child(&kind)
}
