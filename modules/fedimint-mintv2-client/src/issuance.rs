use bitcoin_hashes::sha256;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1::{ecdh, rand, Keypair, PublicKey, SecretKey};
use fedimint_core::{secp256k1, Amount, BitcoinHash};
use fedimint_mintv2_common::{MintOutput, MintOutputV0};
use tbs::{
    blind_message, unblind_signature, BlindedMessage, BlindedSignature, BlindingKey, Message,
};

use crate::SpendableNote;

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, Encodable, Decodable)]
pub struct NoteIssuanceRequest {
    pub amount: Amount,
    pub tweak: [u8; 32],
    pub ephemeral_pk: PublicKey,
}

fn generate_ephemeral_tweak(static_pk: PublicKey) -> ([u8; 32], PublicKey) {
    let keypair = Keypair::new(secp256k1::SECP256K1, &mut rand::thread_rng());

    let tweak = ecdh::SharedSecret::new(&static_pk, &keypair.secret_key());

    (tweak.secret_bytes(), keypair.public_key())
}

impl NoteIssuanceRequest {
    pub fn new(amount: Amount, static_pk: PublicKey) -> NoteIssuanceRequest {
        let (tweak, ephemeral_pk) = generate_ephemeral_tweak(static_pk);

        NoteIssuanceRequest {
            amount,
            tweak,
            ephemeral_pk,
        }
    }

    pub fn recover(output: MintOutputV0, static_keypair: Keypair) -> Option<NoteIssuanceRequest> {
        let tweak = ecdh::SharedSecret::new(&output.ephemeral_pk, &static_keypair.secret_key());

        let request = NoteIssuanceRequest {
            amount: output.amount,
            tweak: tweak.secret_bytes(),
            ephemeral_pk: output.ephemeral_pk,
        };

        if request.blinded_message() != output.nonce {
            return None;
        }

        Some(request)
    }

    pub fn blinded_message(&self) -> BlindedMessage {
        blind_message(self.message(), self.blinding_key())
    }

    pub fn keypair(&self) -> Keypair {
        SecretKey::from_slice(&self.tweak)
            .expect("32 bytes, within curve order")
            .keypair(secp256k1::SECP256K1)
    }

    fn blinding_key(&self) -> BlindingKey {
        BlindingKey::from_seed(self.tweak.consensus_hash::<sha256::Hash>().as_byte_array())
    }

    fn message(&self) -> Message {
        Message::from_bytes_sha256(&self.keypair().public_key().serialize())
    }

    pub fn output(&self) -> MintOutput {
        MintOutput::new_v0(self.amount, self.blinded_message(), self.ephemeral_pk)
    }

    pub fn finalize(&self, blinded_signature: BlindedSignature) -> SpendableNote {
        SpendableNote {
            amount: self.amount,
            signature: unblind_signature(self.blinding_key(), blinded_signature),
            keypair: self.keypair(),
        }
    }
}
