//! Mnemonic + per-module key derivation.
//!
//! Per-module derivation chain:
//! ```text
//! mnemonic
//!   → seed → HMAC with PICOMINT_CLIENT_NONCE   // DerivableSecret level 0
//!   → federation_key(fed_id)                    // per-federation root
//!   → child(ModuleKind as u64)                  // per-module
//! ```

pub use bip39::{Language, Mnemonic};
use picomint_core::config::FederationId;
use picomint_core::core::ModuleKind;
use picomint_derive_secret::{ChildId, DerivableSecret};
use rand::{CryptoRng, RngCore};

const PICOMINT_CLIENT_NONCE: &[u8] = b"Picomint Client Salt";
const EMPTY_PASSPHRASE: &str = "";
const WORD_COUNT: usize = 12;

/// Generate a fresh 12-word English BIP39 mnemonic.
pub fn random<R: RngCore + CryptoRng>(rng: &mut R) -> Mnemonic {
    Mnemonic::generate_in_with(rng, Language::English, WORD_COUNT)
        .expect("Failed to generate mnemonic, bad word count")
}

/// Derive the per-federation client root secret. Module secrets are derived
/// from this via [`derive_module_secret`].
pub(crate) fn derive_root_secret(
    mnemonic: &Mnemonic,
    federation_id: FederationId,
) -> DerivableSecret {
    DerivableSecret::new_root(
        mnemonic.to_seed_normalized(EMPTY_PASSPHRASE).as_ref(),
        PICOMINT_CLIENT_NONCE,
    )
    .federation_key(&federation_id)
}

pub(crate) fn derive_module_secret(root: &DerivableSecret, kind: ModuleKind) -> DerivableSecret {
    assert_eq!(root.level(), 0);
    root.child_key(ChildId(kind as u64))
}
