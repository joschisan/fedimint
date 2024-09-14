use std::hash::Hash;

use bitcoin::block::Header;
use bitcoin::hashes::sha256d;
use bitcoin::{Amount, Block, BlockHash, OutPoint, ScriptBuf, Transaction};
use fedimint_core::encoding::{Decodable, Encodable};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// A proof about a script owning a certain output. Verifiable using headers
/// only.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
pub struct PegInProof {
    header: Header,
    siblings: Vec<Sibling>,
    transaction: Transaction,
    output_index: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Encodable, Decodable)]
enum Sibling {
    Left(sha256d::Hash),
    Right(sha256d::Hash),
}

impl PegInProof {
    pub fn new(block: &Block, transaction_index: u32, output_index: u32) -> Self {
        let transaction = block
            .txdata
            .get(transaction_index as usize)
            .expect("Transaction index is out of bounds")
            .clone();

        Self {
            header: block.header,
            siblings: construct_merkle_proof(&block.txdata, transaction_index as usize),
            transaction,
            output_index,
        }
    }

    pub fn verify(&self) -> Result<(), PegInProofError> {
        let mut hash = self.transaction.txid().to_raw_hash();

        for sibling in &self.siblings {
            hash = match sibling {
                Sibling::Left(sibling) => (*sibling, hash).consensus_hash::<sha256d::Hash>(),
                Sibling::Right(sibling) => (hash, *sibling).consensus_hash::<sha256d::Hash>(),
            }
        }

        if hash != self.header.merkle_root.to_raw_hash() {
            return Err(PegInProofError::InvalidMerkleProof);
        }

        if self
            .transaction
            .output
            .get(self.output_index as usize)
            .is_none()
        {
            return Err(PegInProofError::OutputIndexOutOfRange);
        }

        Ok(())
    }

    pub fn outpoint(&self) -> bitcoin::OutPoint {
        OutPoint {
            txid: self.transaction.txid(),
            vout: self.output_index,
        }
    }

    pub fn block_hash(&self) -> BlockHash {
        self.header.block_hash()
    }

    pub fn script(&self) -> ScriptBuf {
        self.transaction
            .output
            .get(self.output_index as usize)
            .expect("PegInProof has been verified")
            .script_pubkey
            .clone()
    }

    pub fn amount(&self) -> Amount {
        Amount::from_sat(
            self.transaction
                .output
                .get(self.output_index as usize)
                .expect("PegInProof has been verified")
                .value,
        )
    }
}

#[derive(Debug, Error, Encodable, Decodable, Hash, Clone, Eq, PartialEq)]
pub enum PegInProofError {
    #[error("The merkle proof is invalid")]
    InvalidMerkleProof,
    #[error("The output to does not exist")]
    OutputIndexOutOfRange,
}

fn construct_merkle_proof(transactions: &Vec<Transaction>, mut index: usize) -> Vec<Sibling> {
    assert!(index < transactions.len());

    let mut hashes = transactions
        .iter()
        .map(|tx| tx.txid().to_raw_hash())
        .collect::<Vec<sha256d::Hash>>();

    let mut proof = Vec::new();

    while hashes.len() > 1 {
        if index & 1 == 0 {
            proof.push(Sibling::Right(hashes[index + 1]))
        } else {
            proof.push(Sibling::Left(hashes[index - 1]))
        }

        index /= 2;

        hashes = hashes
            .chunks(2)
            .map(|chunk| (chunk[0], *chunk.get(1).unwrap_or(&chunk[0])).consensus_hash())
            .collect();
    }

    // We verify the root we calculated against the bitcoin crate
    assert_eq!(
        bitcoin::merkle_tree::calculate_root(transactions.iter().map(|tx| tx.txid()))
            .expect("Transactions cannot be empty")
            .to_raw_hash(),
        hashes[0]
    );

    proof
}

#[test]
fn test_merkle_proof() {
    use bitcoin::hashes::hex::FromHex;

    // Block 80000
    let block_bytes = Vec::from_hex(
        "01000000ba8b9cda965dd8e536670f9ddec10e53aab14b20bacad2\
    7b9137190000000000190760b278fe7b8565fda3b968b918d5fd997f993b23674c0af3b6fde300b38f33\
    a5914ce6ed5b1b01e32f5702010000000100000000000000000000000000000000000000000000000000\
    00000000000000ffffffff0704e6ed5b1b014effffffff0100f2052a01000000434104b68a50eaa0287e\
    ff855189f949c1c6e5f58b37c88231373d8a59809cbae83059cc6469d65c665ccfd1cfeb75c6e8e19413\
    bba7fbff9bc762419a76d87b16086eac000000000100000001a6b97044d03da79c005b20ea9c0e1a6d9d\
    c12d9f7b91a5911c9030a439eed8f5000000004948304502206e21798a42fae0e854281abd38bacd1aee\
    d3ee3738d9e1446618c4571d1090db022100e2ac980643b0b82c0e88ffdfec6b64e3e6ba35e7ba5fdd7d\
    5d6cc8d25c6b241501ffffffff0100f2052a010000001976a914404371705fa9bd789a2fcd52d2c580b6\
    5d35549d88ac00000000",
    )
    .unwrap();

    let block: Block = bitcoin::consensus::deserialize(&block_bytes).unwrap();

    let pegin_proof = PegInProof::new(&block, 0, 0);

    assert!(pegin_proof.verify().is_ok());

    let pegin_proof = PegInProof::new(&block, 1, 0);

    assert!(pegin_proof.verify().is_ok());
}
