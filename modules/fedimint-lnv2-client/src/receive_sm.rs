use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::OutPoint;
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1::Keypair;
use fedimint_redb::WriteTxRef;
use fedimint_lnv2_common::contracts::IncomingContract;
use fedimint_lnv2_common::{LightningInput, LightningInputV0};
use fedimint_logging::LOG_CLIENT_MODULE_LNV2;
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

impl ReceiveStateMachine {
    pub fn update(&self, state: ReceiveSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

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
    Claiming(Vec<OutPoint>),
    Expired,
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that waits on the receipt of a Lightning payment.
///
/// ```mermaid
/// graph LR
/// classDef virtual fill:#fff,stroke-dasharray: 5 5
///
///     Pending -- incoming contract is confirmed --> Claiming
///     Pending -- decryption contract expires --> Expired
/// ```
impl StateMachine for ReceiveStateMachine {
    const TABLE_NAME: &'static str = "receive-sm";

    type Context = LightningClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            ReceiveSMState::Pending => {
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
            ReceiveSMState::Claiming(..) | ReceiveSMState::Expired => vec![],
        }
    }
}

#[instrument(target = LOG_CLIENT_MODULE_LNV2, skip(ctx))]
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
) -> ReceiveStateMachine {
    let Some(outpoint) = outpoint else {
        return old_state.update(ReceiveSMState::Expired);
    };

    let client_input = ClientInput::<LightningInput> {
        input: LightningInput::V0(LightningInputV0::Incoming(
            outpoint,
            old_state.common.agg_decryption_key,
        )),
        amount: old_state.common.contract.commitment.amount,
        keys: vec![old_state.common.claim_keypair],
    };

    let change_range = ctx
        .client_ctx
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
            ReceivePaymentEvent {
                operation_id: old_state.common.operation_id,
                amount: old_state.common.contract.commitment.amount,
            },
        )
        .await;

    old_state.update(ReceiveSMState::Claiming(change_range.into_iter().collect()))
}
