use bitcoin::key::Keypair;
use bitcoin::secp256k1;
use fedimint_core::Amount;
use fedimint_core::core::{
    DynInput, DynOutput, IInput, IOutput, IntoDynInstance, ModuleInstanceId,
};
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::transaction::{Transaction, TransactionSignature};
use fedimint_logging::LOG_CLIENT;
use itertools::multiunzip;
use rand::{CryptoRng, Rng, RngCore};
use secp256k1::Secp256k1;
use tracing::warn;

use crate::{
    InstancelessDynClientInput, InstancelessDynClientInputBundle, InstancelessDynClientOutput,
    InstancelessDynClientOutputBundle,
};

#[derive(Clone, Debug)]
pub struct ClientInput<I = DynInput> {
    pub input: I,
    pub keys: Vec<Keypair>,
    pub amount: Amount,
}

/// A group of inputs contributed to a transaction.
///
/// State machines that track these inputs (e.g. to handle refunds) are
/// spawned by the module directly — either in its entry-point methods
/// (after `finalize_and_submit_transaction_dbtx` returns the txid) or via
/// the primary-module `FinalContribution::spawn_sms` callback.
#[derive(Clone, Debug)]
pub struct ClientInputBundle<I = DynInput> {
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
    I: IInput + MaybeSend + MaybeSync + 'static,
{
    pub fn into_instanceless(self) -> InstancelessDynClientInputBundle {
        InstancelessDynClientInputBundle {
            inputs: self
                .inputs
                .into_iter()
                .map(|input| InstancelessDynClientInput {
                    input: Box::new(input.input),
                    keys: input.keys,
                    amount: input.amount,
                })
                .collect(),
        }
    }
}

impl<I> IntoDynInstance for ClientInput<I>
where
    I: IntoDynInstance<DynType = DynInput> + 'static,
{
    type DynType = ClientInput;

    fn into_dyn(self, module_instance_id: ModuleInstanceId) -> ClientInput {
        ClientInput {
            input: self.input.into_dyn(module_instance_id),
            keys: self.keys,
            amount: self.amount,
        }
    }
}

impl IntoDynInstance for InstancelessDynClientInputBundle {
    type DynType = ClientInputBundle;

    fn into_dyn(self, module_instance_id: ModuleInstanceId) -> ClientInputBundle {
        ClientInputBundle {
            inputs: self
                .inputs
                .into_iter()
                .map(|input| ClientInput {
                    input: DynInput::from_parts(module_instance_id, input.input),
                    keys: input.keys,
                    amount: input.amount,
                })
                .collect::<Vec<ClientInput>>(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct ClientOutput<O = DynOutput> {
    pub output: O,
    pub amount: Amount,
}

/// A group of outputs contributed to a transaction.
///
/// See [`ClientInputBundle`] for the SM spawning story.
#[derive(Clone, Debug)]
pub struct ClientOutputBundle<O = DynOutput> {
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
    O: IOutput + MaybeSend + MaybeSync + 'static,
{
    pub fn into_instanceless(self) -> InstancelessDynClientOutputBundle {
        InstancelessDynClientOutputBundle {
            outputs: self
                .outputs
                .into_iter()
                .map(|output| InstancelessDynClientOutput {
                    output: Box::new(output.output),
                    amount: output.amount,
                })
                .collect(),
        }
    }
}

impl<O> IntoDynInstance for ClientOutput<O>
where
    O: IntoDynInstance<DynType = DynOutput> + 'static,
{
    type DynType = ClientOutput;

    fn into_dyn(self, module_instance_id: ModuleInstanceId) -> ClientOutput {
        ClientOutput {
            output: self.output.into_dyn(module_instance_id),
            amount: self.amount,
        }
    }
}

impl IntoDynInstance for InstancelessDynClientOutputBundle {
    type DynType = ClientOutputBundle;

    fn into_dyn(self, module_instance_id: ModuleInstanceId) -> ClientOutputBundle {
        ClientOutputBundle {
            outputs: self
                .outputs
                .into_iter()
                .map(|output| ClientOutput {
                    output: DynOutput::from_parts(module_instance_id, output.output),
                    amount: output.amount,
                })
                .collect::<Vec<ClientOutput>>(),
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
    pub fn build<C, R: RngCore + CryptoRng>(
        self,
        secp_ctx: &Secp256k1<C>,
        mut rng: R,
    ) -> Transaction
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

        let nonce: [u8; 8] = rng.r#gen();

        let txid = Transaction::tx_hash_from_parts(&inputs, &outputs, nonce);
        let msg = secp256k1::Message::from_digest_slice(&txid[..]).expect("txid has right length");

        let signatures = input_keys
            .iter()
            .flatten()
            .map(|keypair| secp_ctx.sign_schnorr(&msg, keypair))
            .collect();

        Transaction {
            inputs,
            outputs,
            nonce,
            signatures: TransactionSignature::NaiveMultisig(signatures),
        }
    }

    pub fn inputs(&self) -> impl Iterator<Item = &ClientInput> {
        self.inputs.iter().flat_map(|i| i.inputs.iter())
    }

    pub fn outputs(&self) -> impl Iterator<Item = &ClientOutput> {
        self.outputs.iter().flat_map(|i| i.outputs.iter())
    }
}
