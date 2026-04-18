use picomint_client_module::executor::StateMachine;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;

use super::FinalReceiveState;
use super::events::CompleteEvent;
use crate::{
    GwV2SmContext, InterceptPaymentResponse, PaymentAction, Preimage, await_receive_from_log,
};

/// State machine that completes the incoming payment by contacting the
/// lightning node when the incoming contract has been funded and the preimage
/// is available.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct CompleteStateMachine {
    pub common: CompleteSMCommon,
    pub state: CompleteSMState,
}

picomint_redb::consensus_value!(CompleteStateMachine);

impl CompleteStateMachine {
    pub fn update(&self, state: CompleteSMState) -> Self {
        Self {
            common: self.common.clone(),
            state,
        }
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct CompleteSMCommon {
    pub operation_id: OperationId,
    pub payment_hash: bitcoin::hashes::sha256::Hash,
    pub incoming_chan_id: u64,
    pub htlc_id: u64,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum CompleteSMState {
    Pending,
    Completing(FinalReceiveState),
}

/// Outcome produced by [`CompleteStateMachine::trigger`]. Variant matches
/// current state:
/// - `Pending`       → [`CompleteOutcome::FinalState`]
/// - `Completing(_)` → [`CompleteOutcome::Completed`]
pub enum CompleteOutcome {
    FinalState(FinalReceiveState),
    Completed,
}

impl StateMachine for CompleteStateMachine {
    const TABLE_NAME: &'static str = "complete-sm";

    type Context = GwV2SmContext;
    type Outcome = CompleteOutcome;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        match &self.state {
            CompleteSMState::Pending => CompleteOutcome::FinalState(
                await_receive_from_log(&ctx.client_ctx, self.common.operation_id).await,
            ),
            CompleteSMState::Completing(final_state) => {
                let action = if let FinalReceiveState::Success(preimage) = final_state {
                    PaymentAction::Settle(Preimage(*preimage))
                } else {
                    PaymentAction::Cancel
                };

                ctx.gateway
                    .complete_htlc(InterceptPaymentResponse {
                        incoming_chan_id: self.common.incoming_chan_id,
                        htlc_id: self.common.htlc_id,
                        payment_hash: self.common.payment_hash,
                        action,
                    })
                    .await;

                CompleteOutcome::Completed
            }
        }
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        match outcome {
            CompleteOutcome::FinalState(final_state) => {
                Some(self.update(CompleteSMState::Completing(final_state)))
            }
            CompleteOutcome::Completed => {
                ctx.client_ctx
                    .log_event(dbtx, self.common.operation_id, CompleteEvent)
                    .await;
                None
            }
        }
    }
}
