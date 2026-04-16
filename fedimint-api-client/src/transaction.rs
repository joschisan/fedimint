//! Wire-level transaction and consensus-item types shared between client and
//! server. Previously lived in `fedimint-core::transaction` / `epoch.rs`;
//! moved here with the module-system rip so we can reference static module
//! Input/Output/ConsensusItem enums without creating a cycle through
//! fedimint-core.

use std::fmt;

use bitcoin::hashes::Hash;
use bitcoin::hex::DisplayHex as _;
use fedimint_core::core::{DynInput, DynInputError, DynOutput, DynOutputError};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::SerdeModuleEncoding;
use fedimint_core::{Amount, TransactionId};
use thiserror::Error;

/// An atomic value transfer operation within the Fedimint system and consensus.
///
/// The mint enforces that the total value of the outputs equals the total value
/// of the inputs, to prevent creating funds out of thin air. In some cases, the
/// value of the inputs and outputs can both be 0 e.g. when creating an offer to
/// a Lightning Gateway.
#[derive(Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub struct Transaction {
    pub inputs: Vec<DynInput>,
    pub outputs: Vec<DynOutput>,
    pub nonce: [u8; 8],
    pub signatures: TransactionSignature,
}

impl fmt::Debug for Transaction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Transaction")
            .field("txid", &self.tx_hash())
            .field("inputs", &self.inputs)
            .field("outputs", &self.outputs)
            .field("nonce", &self.nonce)
            .field("signatures", &self.signatures)
            .finish()
    }
}

pub type SerdeTransaction = SerdeModuleEncoding<Transaction>;

impl Transaction {
    pub const MAX_TX_SIZE: usize = fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT - 32;

    pub fn tx_hash(&self) -> TransactionId {
        Self::tx_hash_from_parts(&self.inputs, &self.outputs, self.nonce)
    }

    pub fn tx_hash_from_parts(
        inputs: &[DynInput],
        outputs: &[DynOutput],
        nonce: [u8; 8],
    ) -> TransactionId {
        let mut engine = TransactionId::engine();
        inputs
            .consensus_encode(&mut engine)
            .expect("write to hash engine can't fail");
        outputs
            .consensus_encode(&mut engine)
            .expect("write to hash engine can't fail");
        nonce
            .consensus_encode(&mut engine)
            .expect("write to hash engine can't fail");
        TransactionId::from_engine(engine)
    }

    pub fn validate_signatures(
        &self,
        pub_keys: &[fedimint_core::secp256k1::PublicKey],
    ) -> Result<(), TransactionError> {
        use fedimint_core::secp256k1;

        let signatures = match &self.signatures {
            TransactionSignature::NaiveMultisig(sigs) => sigs,
            TransactionSignature::Default { variant, .. } => {
                return Err(TransactionError::UnsupportedSignatureScheme { variant: *variant });
            }
        };

        if pub_keys.len() != signatures.len() {
            return Err(TransactionError::InvalidWitnessLength);
        }

        let txid = self.tx_hash();
        let msg = secp256k1::Message::from_digest_slice(&txid[..]).expect("txid has right length");

        for (pk, signature) in pub_keys.iter().zip(signatures) {
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

#[derive(Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub enum TransactionSignature {
    NaiveMultisig(Vec<fedimint_core::secp256k1::schnorr::Signature>),
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}

impl fmt::Debug for TransactionSignature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NaiveMultisig(multi) => {
                f.debug_struct("NaiveMultisig")
                    .field("len", &multi.len())
                    .finish()?;
            }
            Self::Default { variant, bytes } => {
                f.debug_struct("TransactionSignature::Default")
                    .field("variant", variant)
                    .field("bytes", &bytes.as_hex())
                    .finish()?;
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
    #[error("The transaction's signature scheme is not supported: variant={variant}")]
    UnsupportedSignatureScheme { variant: u64 },
    #[error("The transaction did not have the correct number of signatures")]
    InvalidWitnessLength,
    #[error("The transaction had an invalid input: {}", .0)]
    Input(DynInputError),
    #[error("The transaction had an invalid output: {}", .0)]
    Output(DynOutputError),
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
    Module(fedimint_core::core::DynModuleConsensusItem),
    /// Allows us to add new items in the future without crashing old clients
    /// that try to interpret the session log.
    #[encodable_default]
    Default { variant: u64, bytes: Vec<u8> },
}
