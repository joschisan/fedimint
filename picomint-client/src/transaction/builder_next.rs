//! New transaction builder. Will replace `builder.rs` once all callers have
//! migrated; for now lives alongside it so the migration can land in pieces.

use bitcoin::key::Keypair;
use bitcoin::secp256k1::{self, Secp256k1};
use bitcoin_hashes::Hash;
use picomint_core::Amount;
use picomint_core::transaction::Transaction;
use picomint_core::wire;


#[derive(Clone, Debug)]
pub struct Input {
    pub input: wire::Input,
    pub keypair: Keypair,
    pub amount: Amount,
    pub fee: Amount,
}

#[derive(Clone, Debug)]
pub struct Output {
    pub output: wire::Output,
    pub amount: Amount,
    pub fee: Amount,
}

#[derive(Default, Clone, Debug)]
pub struct TransactionBuilder {
    inputs: Vec<Input>,
    outputs: Vec<Output>,
}

impl TransactionBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_input(input: Input) -> Self {
        Self {
            inputs: vec![input],
            outputs: Vec::new(),
        }
    }

    pub fn from_output(output: Output) -> Self {
        Self {
            inputs: Vec::new(),
            outputs: vec![output],
        }
    }

    pub fn add_input(&mut self, input: Input) {
        self.inputs.push(input);
    }

    pub fn add_output(&mut self, output: Output) {
        self.outputs.push(output);
    }

    pub fn input_amount(&self) -> Amount {
        self.inputs.iter().map(|i| i.amount).sum()
    }

    pub fn output_amount(&self) -> Amount {
        self.outputs.iter().map(|o| o.amount).sum()
    }

    pub fn total_fee(&self) -> Amount {
        self.inputs.iter().map(|i| i.fee).sum::<Amount>() + self.outputs.iter().map(|o| o.fee).sum()
    }

    pub fn build(self) -> Transaction {
        let secp_ctx = Secp256k1::new();

        let inputs: Vec<wire::Input> = self.inputs.iter().map(|i| i.input.clone()).collect();
        let outputs: Vec<wire::Output> = self.outputs.into_iter().map(|o| o.output).collect();

        let txid = Transaction::tx_hash_from_parts(&inputs, &outputs);

        let message = secp256k1::Message::from_digest(txid.as_raw_hash().to_byte_array());

        let signatures = self
            .inputs
            .iter()
            .map(|i| secp_ctx.sign_schnorr(&message, &i.keypair))
            .collect();

        Transaction {
            inputs,
            outputs,
            signatures,
        }
    }
}
