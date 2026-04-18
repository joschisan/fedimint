use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_client_module::transaction::{ClientInput, ClientInputBundle};
use picomint_core::OutPoint;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::secp256k1::Keypair;
use picomint_ln_common::LightningInput;
use picomint_ln_common::contracts::IncomingContract;
use picomint_logging::LOG_CLIENT_MODULE_LN;
use picomint_redb::WriteTxRef;
use tpe::AggregateDecryptionKey;
use tracing::instrument;

use crate::LightningClientContext;
use crate::api::LightningFederationApi;
use crate::events::ReceivePaymentEvent;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveStateMachine {
    pub common: ReceiveSMCommon,
    pub state: ReceiveSMState,
}

picomint_redb::consensus_value!(ReceiveStateMachine);

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveSMCommon {
    pub operation_id: OperationId,
    pub contract: IncomingContract,
    pub claim_keypair: Keypair,
    pub agg_decryption_key: AggregateDecryptionKey,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Pending,
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that waits on the receipt of a Lightning payment. Terminates
/// when the incoming contract is either claimed or expires.
impl StateMachine for ReceiveStateMachine {
    const TABLE_NAME: &'static str = "receive-sm";

    type Context = LightningClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx_clone = ctx.clone();
        let contract = self.common.contract.clone();
        vec![SmStateTransition::new(
            await_incoming_contract_sm(contract, ctx_clone.clone()),
            move |dbtx, outpoint, old_state| {
                let ctx = ctx_clone.clone();
                Box::pin(transition_incoming_contract_sm(
                    ctx, dbtx, old_state, outpoint,
                ))
            },
        )]
    }
}

#[instrument(target = LOG_CLIENT_MODULE_LN, skip(ctx))]
async fn await_incoming_contract_sm(
    contract: IncomingContract,
    ctx: LightningClientContext,
) -> Option<OutPoint> {
    ctx.client_ctx
        .module_api()
        .await_incoming_contract(&contract.contract_id(), contract.commitment.expiration)
        .await
}

async fn transition_incoming_contract_sm(
    ctx: LightningClientContext,
    dbtx: &WriteTxRef<'_>,
    old_state: ReceiveStateMachine,
    outpoint: Option<OutPoint>,
) -> Option<ReceiveStateMachine> {
    let outpoint = outpoint?;

    let client_input = ClientInput::<LightningInput> {
        input: LightningInput::Incoming(outpoint, old_state.common.agg_decryption_key),
        amount: old_state.common.contract.commitment.amount,
        keys: vec![old_state.common.claim_keypair],
    };

    ctx.client_ctx
        .claim_inputs(
            dbtx,
            ClientInputBundle::new(vec![client_input]),
            old_state.common.operation_id,
        )
        .await
        .expect("Cannot claim input, additional funding needed");

    ctx.client_ctx
        .log_event(
            dbtx,
            old_state.common.operation_id,
            ReceivePaymentEvent {
                amount: old_state.common.contract.commitment.amount,
            },
        )
        .await;

    None
}
