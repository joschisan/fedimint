//! Wire-level transaction and consensus-item types shared between client and
//! server. Previously lived in `fedimint-core::transaction` / `epoch.rs`;
//! moved here with the module-system rip so we can reference static module
//! Input/Output/ConsensusItem enums without creating a cycle through
//! fedimint-core.

use std::fmt;

use bitcoin::hashes::Hash;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::SerdeModuleEncoding;
use fedimint_core::{Amount, TransactionId};
use thiserror::Error;

use crate::wire;

/// An atomic value transfer operation within the Fedimint system and consensus.
///
/// The mint enforces that the total value of the outputs equals the total value
/// of the inputs, to prevent creating funds out of thin air. In some cases, the
/// value of the inputs and outputs can both be 0 e.g. when creating an offer to
/// a Lightning Gateway.
#[derive(Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub struct Transaction {
    pub inputs: Vec<wire::Input>,
    pub outputs: Vec<wire::Output>,
    pub signatures: Vec<fedimint_core::secp256k1::schnorr::Signature>,
}

impl fmt::Debug for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction")
            .field("txid", &self.tx_hash())
            .field("inputs", &self.inputs)
            .field("outputs", &self.outputs)
            .field("signatures", &self.signatures)
            .finish()
    }
}

pub type SerdeTransaction = SerdeModuleEncoding<Transaction>;

impl Transaction {
    pub const MAX_TX_SIZE: usize = fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT - 32;

    pub fn tx_hash(&self) -> TransactionId {
        Self::tx_hash_from_parts(&self.inputs, &self.outputs)
    }

    pub fn tx_hash_from_parts(inputs: &[wire::Input], outputs: &[wire::Output]) -> TransactionId {
        let mut engine = TransactionId::engine();
        inputs
            .consensus_encode(&mut engine)
            .expect("write to hash engine can't fail");
        outputs
            .consensus_encode(&mut engine)
            .expect("write to hash engine can't fail");
        TransactionId::from_engine(engine)
    }

    pub fn validate_signatures(
        &self,
        pub_keys: &[fedimint_core::secp256k1::PublicKey],
    ) -> Result<(), TransactionError> {
        use fedimint_core::secp256k1;

        if pub_keys.len() != self.signatures.len() {
            return Err(TransactionError::InvalidWitnessLength);
        }

        let txid = self.tx_hash();
        let msg = secp256k1::Message::from_digest_slice(&txid[..]).expect("txid has right length");

        for (pk, signature) in pub_keys.iter().zip(&self.signatures) {
            if secp256k1::global::SECP256K1
                .verify_schnorr(signature, &msg, &pk.x_only_public_key().0)
                .is_err()
            {
                return Err(TransactionError::InvalidSignature {
                    tx: self.consensus_encode_to_hex(),
                    hash: self.tx_hash().consensus_encode_to_hex(),
                    sig: signature.consensus_encode_to_hex(),
                    key: pk.consensus_encode_to_hex(),
                });
            }
        }

        Ok(())
    }
}

#[derive(Debug, Error, Encodable, Decodable, Clone, Eq, PartialEq)]
pub enum TransactionError {
    #[error("The transaction is unbalanced (in={inputs}, out={outputs}, fee={fee})")]
    UnbalancedTransaction {
        inputs: Amount,
        outputs: Amount,
        fee: Amount,
    },
    #[error("The transaction's signature is invalid: tx={tx}, hash={hash}, sig={sig}, key={key}")]
    InvalidSignature {
        tx: String,
        hash: String,
        sig: String,
        key: String,
    },
    #[error("The transaction did not have the correct number of signatures")]
    InvalidWitnessLength,
    #[error("The transaction had an invalid input: {}", .0)]
    Input(wire::InputError),
    #[error("The transaction had an invalid output: {}", .0)]
    Output(wire::OutputError),
}

pub const TRANSACTION_OVERFLOW_ERROR: TransactionError = TransactionError::UnbalancedTransaction {
    inputs: Amount::ZERO,
    outputs: Amount::ZERO,
    fee: Amount::ZERO,
};

#[derive(Debug, Encodable, Decodable, Clone, Eq, PartialEq)]
pub struct TransactionSubmissionOutcome(pub Result<TransactionId, TransactionError>);

/// All the items that may be produced during a consensus epoch.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum ConsensusItem {
    /// Threshold sign the epoch history for verification via the API
    Transaction(Transaction),
    /// Any data that modules require consensus on
    Module(wire::ModuleConsensusItem),
}
