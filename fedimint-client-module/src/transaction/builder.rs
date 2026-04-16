use bitcoin::key::Keypair;
use bitcoin::secp256k1;
use fedimint_api_client::transaction::Transaction;
use fedimint_api_client::wire;
use fedimint_core::Amount;
use fedimint_logging::LOG_CLIENT;
use itertools::multiunzip;
use secp256k1::Secp256k1;
use tracing::warn;

#[derive(Clone, Debug)]
pub struct ClientInput<I = wire::Input> {
    pub input: I,
    pub keys: Vec<Keypair>,
    pub amount: Amount,
}

impl<I> ClientInput<I>
where
    wire::Input: From<I>,
{
    pub fn into_wire(self) -> ClientInput<wire::Input> {
        ClientInput {
            input: self.input.into(),
            keys: self.keys,
            amount: self.amount,
        }
    }
}

/// A group of inputs contributed to a transaction.
#[derive(Clone, Debug)]
pub struct ClientInputBundle<I = wire::Input> {
    pub(crate) inputs: Vec<ClientInput<I>>,
}

impl<I> ClientInputBundle<I> {
    pub fn new(inputs: Vec<ClientInput<I>>) -> Self {
        Self { inputs }
    }

    pub fn inputs(&self) -> &[ClientInput<I>] {
        &self.inputs
    }

    pub fn is_empty(&self) -> bool {
        self.inputs.is_empty()
    }
}

impl<I> ClientInputBundle<I>
where
    wire::Input: From<I>,
{
    pub fn into_wire(self) -> ClientInputBundle<wire::Input> {
        ClientInputBundle {
            inputs: self
                .inputs
                .into_iter()
                .map(ClientInput::into_wire)
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientOutput<O = wire::Output> {
    pub output: O,
    pub amount: Amount,
}

impl<O> ClientOutput<O>
where
    wire::Output: From<O>,
{
    pub fn into_wire(self) -> ClientOutput<wire::Output> {
        ClientOutput {
            output: self.output.into(),
            amount: self.amount,
        }
    }
}

/// A group of outputs contributed to a transaction.
#[derive(Clone, Debug)]
pub struct ClientOutputBundle<O = wire::Output> {
    pub(crate) outputs: Vec<ClientOutput<O>>,
}

impl<O> ClientOutputBundle<O> {
    pub fn new(outputs: Vec<ClientOutput<O>>) -> Self {
        if outputs.is_empty() {
            warn!(target: LOG_CLIENT, "Empty output bundle will be illegal in the future");
        }
        Self { outputs }
    }

    pub fn outputs(&self) -> &[ClientOutput<O>] {
        &self.outputs
    }

    pub fn is_empty(&self) -> bool {
        self.outputs.is_empty()
    }

    pub fn with(mut self, other: Self) -> Self {
        self.outputs.extend(other.outputs);
        self
    }
}

impl<O> ClientOutputBundle<O>
where
    wire::Output: From<O>,
{
    pub fn into_wire(self) -> ClientOutputBundle<wire::Output> {
        ClientOutputBundle {
            outputs: self
                .outputs
                .into_iter()
                .map(ClientOutput::into_wire)
                .collect(),
        }
    }
}

#[derive(Default, Clone, Debug)]
pub struct TransactionBuilder {
    inputs: Vec<ClientInputBundle>,
    outputs: Vec<ClientOutputBundle>,
}

impl TransactionBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_inputs(mut self, inputs: ClientInputBundle) -> Self {
        self.inputs.push(inputs);
        self
    }

    pub fn with_outputs(mut self, outputs: ClientOutputBundle) -> Self {
        self.outputs.push(outputs);
        self
    }

    /// Build the signed `Transaction`. Signature collection happens here;
    /// all SM spawning now lives outside the builder.
    pub fn build<C>(self, secp_ctx: &Secp256k1<C>) -> Transaction
    where
        C: secp256k1::Signing + secp256k1::Verification,
    {
        let (inputs, input_keys): (Vec<_>, Vec<_>) = multiunzip(
            self.inputs
                .into_iter()
                .flat_map(|bundle| bundle.inputs.into_iter())
                .map(|input| (input.input, input.keys)),
        );

        let outputs: Vec<_> = self
            .outputs
            .into_iter()
            .flat_map(|bundle| bundle.outputs.into_iter())
            .map(|output| output.output)
            .collect();

        let txid = Transaction::tx_hash_from_parts(&inputs, &outputs);
        let msg = secp256k1::Message::from_digest_slice(&txid[..]).expect("txid has right length");

        let signatures = input_keys
            .iter()
            .flatten()
            .map(|keypair| secp_ctx.sign_schnorr(&msg, keypair))
            .collect();

        Transaction {
            inputs,
            outputs,
            signatures,
        }
    }

    pub fn inputs(&self) -> impl Iterator<Item = &ClientInput> {
        self.inputs.iter().flat_map(|i| i.inputs.iter())
    }

    pub fn outputs(&self) -> impl Iterator<Item = &ClientOutput> {
        self.outputs.iter().flat_map(|i| i.outputs.iter())
    }
}
