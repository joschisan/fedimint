use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_core::OutPoint;
use picomint_core::core::OperationId;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;

use crate::WalletClientContext;
use crate::api::WalletFederationApi;
use crate::events::{SendPaymentStatus, SendPaymentUpdateEvent};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendStateMachine {
    pub common: SendSMCommon,
    pub state: SendSMState,
}

picomint_core::consensus_key!(SendStateMachine);

impl SendStateMachine {
    pub fn update(&self, state: SendSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct SendSMCommon {
    pub operation_id: OperationId,
    pub outpoint: OutPoint,
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum SendSMState {
    Funding,
    Success(bitcoin::Txid),
    Aborted(String),
    Failure,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
enum AwaitFundingResult {
    Success(bitcoin::Txid),
    Aborted(String),
    Failure,
}

impl StateMachine for SendStateMachine {
    const TABLE_NAME: &'static str = "send-sm";

    type Context = WalletClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            SendSMState::Funding => {
                let ctx = ctx.clone();
                let operation_id = self.common.operation_id;
                let outpoint = self.common.outpoint;
                vec![SmStateTransition::new(
                    await_funding_sm(ctx.clone(), operation_id, outpoint),
                    move |dbtx, result, old_state| {
                        let ctx = ctx.clone();
                        Box::pin(transition_funding_sm(ctx, dbtx, result, old_state))
                    },
                )]
            }
            SendSMState::Success(_) | SendSMState::Aborted(_) | SendSMState::Failure => vec![],
        }
    }
}

async fn await_funding_sm(
    ctx: WalletClientContext,
    operation_id: OperationId,
    outpoint: OutPoint,
) -> AwaitFundingResult {
    if let Err(error) = ctx
        .client_ctx
        .await_tx_accepted(operation_id, outpoint.txid)
        .await
    {
        return AwaitFundingResult::Aborted(error);
    }

    match ctx.client_ctx.module_api().tx_id(outpoint).await {
        Some(txid) => AwaitFundingResult::Success(txid),
        None => AwaitFundingResult::Failure,
    }
}

async fn transition_funding_sm(
    ctx: WalletClientContext,
    dbtx: &WriteTxRef<'_>,
    result: AwaitFundingResult,
    old_state: SendStateMachine,
) -> SendStateMachine {
    match result {
        AwaitFundingResult::Success(txid) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    SendPaymentUpdateEvent {
                        status: SendPaymentStatus::Success(txid),
                    },
                )
                .await;

            old_state.update(SendSMState::Success(txid))
        }
        AwaitFundingResult::Aborted(error) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    SendPaymentUpdateEvent {
                        status: SendPaymentStatus::Aborted,
                    },
                )
                .await;

            old_state.update(SendSMState::Aborted(error))
        }
        AwaitFundingResult::Failure => old_state.update(SendSMState::Failure),
    }
}
