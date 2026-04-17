use std::fmt::Debug;

use picomint_core::config::FederationId;
use picomint_core::core::ModuleKind;
use picomint_derive_secret::{ChildId, DerivableSecret};

// Derived from federation-root-secret
const TYPE_MODULE: ChildId = ChildId(0);

pub trait DeriveableSecretClientExt {
    fn derive_module_secret(&self, kind: ModuleKind) -> DerivableSecret;
}

impl DeriveableSecretClientExt for DerivableSecret {
    fn derive_module_secret(&self, kind: ModuleKind) -> DerivableSecret {
        assert_eq!(self.level(), 0);
        self.child_key(TYPE_MODULE)
            .child_key(ChildId(kind as u64))
    }
}

/// Trait defining a way to generate, serialize and deserialize a root secret.
/// It defines a `Encoding` associated type which represents a specific
/// representation of a secret (e.g. a bip39, slip39, CODEX32, … struct) and
/// then defines the methods necessary for the client to interact with it.
///
/// We use a strategy pattern (i.e. implementing the trait on a zero sized type
/// with the actual secret struct as an associated type instead of implementing
/// the necessary functions directly on the secret struct) to allow external
/// implementations on third-party types without wrapping them in newtypes.
pub trait RootSecretStrategy: Debug {
    /// Type representing the secret
    type Encoding: Clone;

    /// Conversion function from the external encoding to the internal one
    fn to_root_secret(secret: &Self::Encoding) -> DerivableSecret;

    /// Serialization function for the external encoding
    fn consensus_encode(
        secret: &Self::Encoding,
        writer: &mut impl std::io::Write,
    ) -> std::io::Result<()>;

    /// Deserialization function for the external encoding
    fn consensus_decode(reader: &mut impl std::io::Read) -> std::io::Result<Self::Encoding>;

    /// Random generation function for the external secret type
    fn random<R>(rng: &mut R) -> Self::Encoding
    where
        R: rand::RngCore + rand::CryptoRng;
}

/// Convenience function to derive picomint-client root secret
/// using the default (0) wallet number, given a global root secret
/// that's managed externally by a consumer of picomint-client.
///
/// See docs/secret_derivation.md
///
/// `global_root_secret/<key-type=per-federation=0>/<federation-id>/
/// <wallet-number=0>/<key-type=picomint-client=0>`
pub fn get_default_client_secret(
    global_root_secret: &DerivableSecret,
    federation_id: &FederationId,
) -> DerivableSecret {
    let multi_federation_root_secret = global_root_secret.child_key(ChildId(0));
    let federation_root_secret = multi_federation_root_secret.federation_key(federation_id);
    let federation_wallet_root_secret = federation_root_secret.child_key(ChildId(0)); // wallet-number=0
    federation_wallet_root_secret.child_key(ChildId(0)) // key-type=picomint-client=0
}
