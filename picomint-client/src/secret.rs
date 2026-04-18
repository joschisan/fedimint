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
        self.child_key(TYPE_MODULE).child_key(ChildId(kind as u64))
    }
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
