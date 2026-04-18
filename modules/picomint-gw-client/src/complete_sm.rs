use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;

use super::FinalReceiveState;
use super::events::CompleteLightningPaymentEvent;
use crate::{
    GwV2SmContext, InterceptPaymentResponse, PaymentAction, Preimage, await_receive_from_log,
};

#[cfg_attr(doc, aquamarine::aquamarine)]
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

impl StateMachine for CompleteStateMachine {
    const TABLE_NAME: &'static str = "complete-sm";

    type Context = GwV2SmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            CompleteSMState::Pending => {
                let ctx_clone = ctx.clone();
                let operation_id = self.common.operation_id;
                vec![SmStateTransition::new(
                    async move { await_receive_from_log(&ctx_clone.client_ctx, operation_id).await },
                    |_dbtx: &WriteTxRef<'_>,
                     result: FinalReceiveState,
                     old_state: CompleteStateMachine| {
                        Box::pin(async move {
                            Some(old_state.update(CompleteSMState::Completing(result)))
                        })
                    },
                )]
            }
            CompleteSMState::Completing(final_receive_state) => {
                let ctx_clone = ctx.clone();
                let payment_hash = self.common.payment_hash;
                let incoming_chan_id = self.common.incoming_chan_id;
                let htlc_id = self.common.htlc_id;
                let final_state = final_receive_state.clone();
                vec![SmStateTransition::new(
                    await_completion_sm(
                        ctx_clone.clone(),
                        payment_hash,
                        final_state,
                        incoming_chan_id,
                        htlc_id,
                    ),
                    move |dbtx, (), old_state| {
                        let ctx = ctx_clone.clone();
                        Box::pin(transition_completion_sm(ctx, dbtx, old_state))
                    },
                )]
            }
        }
    }
}

async fn await_completion_sm(
    ctx: GwV2SmContext,
    payment_hash: bitcoin::hashes::sha256::Hash,
    final_receive_state: FinalReceiveState,
    incoming_chan_id: u64,
    htlc_id: u64,
) {
    let action = if let FinalReceiveState::Success(preimage) = final_receive_state {
        PaymentAction::Settle(Preimage(preimage))
    } else {
        PaymentAction::Cancel
    };

    let intercept_htlc_response = InterceptPaymentResponse {
        incoming_chan_id,
        htlc_id,
        payment_hash,
        action,
    };

    ctx.gateway.complete_htlc(intercept_htlc_response).await;
}

async fn transition_completion_sm(
    ctx: GwV2SmContext,
    dbtx: &WriteTxRef<'_>,
    old_state: CompleteStateMachine,
) -> Option<CompleteStateMachine> {
    ctx.client_ctx
        .log_event(
            dbtx,
            old_state.common.operation_id,
            CompleteLightningPaymentEvent,
        )
        .await;

    None
}
