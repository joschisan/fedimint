use fedimint_client::DynGlobalClientContext;
use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::module::ClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_core::TransactionId;
use fedimint_core::core::OperationId;
use fedimint_core::db::DatabaseTransaction;
use fedimint_core::encoding::{Decodable, Encodable};

use crate::events::{ReceivePaymentStatus, ReceivePaymentUpdateEvent};
use crate::{MintClientContext, MintClientModule, MintSmContext};

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
    pub txid: TransactionId,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Pending,
    Success,
    Rejected(String),
}

impl State for ReceiveStateMachine {
    type ModuleContext = MintClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        let client_ctx = context.client_ctx.clone();

        match &self.state {
            ReceiveSMState::Pending => vec![StateTransition::new(
                Self::await_tx_outcome(global_context.clone(), self.common.txid),
                move |dbtx, result, old_state| {
                    Box::pin(Self::transition_tx_outcome(
                        client_ctx.clone(),
                        dbtx,
                        result,
                        old_state,
                    ))
                },
            )],
            ReceiveSMState::Success | ReceiveSMState::Rejected(..) => vec![],
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

impl ReceiveStateMachine {
    async fn await_tx_outcome(
        global_context: DynGlobalClientContext,
        txid: TransactionId,
    ) -> Result<(), String> {
        global_context.await_tx_accepted(txid).await
    }

    async fn transition_tx_outcome(
        client_ctx: ClientContext<MintClientModule>,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        result: Result<(), String>,
        old_state: ReceiveStateMachine,
    ) -> ReceiveStateMachine {
        let (status, new_state) = match result {
            Ok(()) => (ReceivePaymentStatus::Success, ReceiveSMState::Success),
            Err(e) => (ReceivePaymentStatus::Rejected, ReceiveSMState::Rejected(e)),
        };

        client_ctx
            .log_event(
                &mut dbtx.module_tx(),
                ReceivePaymentUpdateEvent {
                    operation_id: old_state.common.operation_id,
                    status,
                },
            )
            .await;

        old_state.update(new_state)
    }
}

// ---- New per-module executor impl ------------------------------------------

impl StateMachine for ReceiveStateMachine {
    const DB_PREFIX: u8 = crate::client_db::DbKeyPrefix::ReceiveStateMachine as u8;

    type Context = MintSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            ReceiveSMState::Pending => {
                let ctx_trigger = ctx.clone();
                let ctx_transition = ctx.clone();
                let txid = self.common.txid;
                vec![SmStateTransition::new(
                    async move { ctx_trigger.client_ctx.await_tx_accepted(txid).await },
                    move |dbtx, result, old_state| {
                        let ctx = ctx_transition.clone();
                        Box::pin(transition_tx_outcome_sm(ctx, dbtx, result, old_state))
                    },
                )]
            }
            ReceiveSMState::Success | ReceiveSMState::Rejected(..) => vec![],
        }
    }
}

async fn transition_tx_outcome_sm(
    ctx: MintSmContext,
    dbtx: &mut DatabaseTransaction<'_>,
    result: Result<(), String>,
    old_state: ReceiveStateMachine,
) -> ReceiveStateMachine {
    let (status, new_state) = match result {
        Ok(()) => (ReceivePaymentStatus::Success, ReceiveSMState::Success),
        Err(e) => (ReceivePaymentStatus::Rejected, ReceiveSMState::Rejected(e)),
    };

    ctx.client_ctx
        .log_event(
            dbtx,
            ReceivePaymentUpdateEvent {
                operation_id: old_state.common.operation_id,
                status,
            },
        )
        .await;

    old_state.update(new_state)
}
