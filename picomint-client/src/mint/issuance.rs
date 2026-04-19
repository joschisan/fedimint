use bitcoin_hashes::{Hash, hash160, sha256};
use picomint_core::mint::{Denomination, MintOutput, nonce_message};
use picomint_core::secp256k1::rand::Rng;
use picomint_core::secp256k1::{Keypair, PublicKey};
use picomint_encoding::{Decodable, Encodable};
use tbs::{BlindedMessage, BlindedSignature, BlindingKey, blind_message, unblind_signature};

use super::{SpendableNote, thread_rng};
use crate::secret::Secret;

#[derive(Encodable)]
pub enum RootSecretPath {
    TweakFilter,
    NoteNonce,
    NoteBlinding,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Encodable, Decodable)]
pub struct NoteIssuanceRequest {
    pub denomination: Denomination,
    pub tweak: [u8; 16],
    pub keypair: Keypair,
    pub blinding_key: BlindingKey,
}

impl NoteIssuanceRequest {
    pub fn new(denomination: Denomination, tweak: [u8; 16], root_secret: &Secret) -> Self {
        let secret = output_secret(denomination, tweak, root_secret);

        Self {
            denomination,
            tweak,
            keypair: keypair(&secret),
            blinding_key: blinding_key(&secret),
        }
    }

    pub fn output(&self) -> MintOutput {
        MintOutput {
            denomination: self.denomination,
            nonce: self.blinded_message(),
            tweak: self.tweak,
        }
    }

    pub fn finalize(&self, signature: BlindedSignature) -> SpendableNote {
        SpendableNote {
            denomination: self.denomination,
            keypair: self.keypair,
            signature: unblind_signature(self.blinding_key, signature),
        }
    }

    pub fn blinded_message(&self) -> BlindedMessage {
        blind_message(nonce_message(self.keypair.public_key()), self.blinding_key)
    }
}

// ============ Grinding Functions ============

pub fn tweak_filter(root_secret: &Secret) -> [u8; 32] {
    root_secret.child(&RootSecretPath::TweakFilter).to_bytes()
}

pub fn grind_tweak(root_secret: &Secret) -> [u8; 16] {
    let filter = tweak_filter(root_secret);

    loop {
        let tweak = thread_rng().r#gen();

        if check_tweak(tweak, filter) {
            return tweak;
        }
    }
}

pub fn check_tweak(tweak: [u8; 16], seed: [u8; 32]) -> bool {
    (seed, tweak)
        .consensus_hash::<sha256::Hash>()
        .to_byte_array()
        .iter()
        .take(2)
        .all(|b| *b == 0)
}

// ============ Validation Functions ============

pub fn check_nonce(secret: &OutputSecret, nonce_hash: hash160::Hash) -> bool {
    blinded_message(secret).consensus_hash::<hash160::Hash>() == nonce_hash
}

// ============ Core Crypto Functions ============

pub struct OutputSecret {
    denomination: Denomination,
    tweak: [u8; 16],
    root: Secret,
}

pub fn output_secret(denomination: Denomination, tweak: [u8; 16], root: &Secret) -> OutputSecret {
    OutputSecret {
        denomination,
        tweak,
        root: *root,
    }
}

fn keypair(secret: &OutputSecret) -> Keypair {
    secret
        .root
        .child(&RootSecretPath::NoteNonce)
        .child(&secret.denomination)
        .child(&secret.tweak)
        .to_secp_keypair()
}

pub fn nonce(secret: &OutputSecret) -> PublicKey {
    keypair(secret).public_key()
}

fn blinding_key(secret: &OutputSecret) -> BlindingKey {
    BlindingKey(
        secret
            .root
            .child(&RootSecretPath::NoteBlinding)
            .child(&secret.denomination)
            .child(&secret.tweak)
            .to_bls_scalar(),
    )
}

pub fn blinded_message(secret: &OutputSecret) -> BlindedMessage {
    blind_message(nonce_message(nonce(secret)), blinding_key(secret))
}
