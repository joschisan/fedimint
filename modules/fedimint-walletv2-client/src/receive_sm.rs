use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_core::TransactionId;
use fedimint_core::core::OperationId;
use fedimint_core::db::DatabaseTransaction;
use fedimint_core::encoding::{Decodable, Encodable};

use crate::WalletClientContext;
use crate::events::{ReceivePaymentStatus, ReceivePaymentUpdateEvent};

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
    pub value: bitcoin::Amount,
    pub fee: bitcoin::Amount,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Funding,
    Success,
    Aborted(String),
}

impl State for ReceiveStateMachine {
    type ModuleContext = WalletClientContext;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        let ctx = context.clone();

        match &self.state {
            ReceiveSMState::Funding => {
                vec![StateTransition::new(
                    Self::await_funding(global_context.clone(), self.common.txid),
                    move |dbtx, result, old_state| {
                        Box::pin(Self::transition_funding(
                            ctx.clone(),
                            dbtx,
                            result,
                            old_state,
                        ))
                    },
                )]
            }
            ReceiveSMState::Success | ReceiveSMState::Aborted(_) => {
                vec![]
            }
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

impl ReceiveStateMachine {
    async fn await_funding(
        global_context: DynGlobalClientContext,
        txid: TransactionId,
    ) -> Result<(), String> {
        global_context.await_tx_accepted(txid).await
    }

    async fn transition_funding(
        context: WalletClientContext,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        result: Result<(), String>,
        old_state: ReceiveStateMachine,
    ) -> ReceiveStateMachine {
        match result {
            Ok(()) => {
                context
                    .client_ctx
                    .log_event(
                        &mut dbtx.module_tx(),
                        ReceivePaymentUpdateEvent {
                            operation_id: old_state.common.operation_id,
                            status: ReceivePaymentStatus::Success,
                        },
                    )
                    .await;

                old_state.update(ReceiveSMState::Success)
            }
            Err(error) => {
                context
                    .client_ctx
                    .log_event(
                        &mut dbtx.module_tx(),
                        ReceivePaymentUpdateEvent {
                            operation_id: old_state.common.operation_id,
                            status: ReceivePaymentStatus::Aborted,
                        },
                    )
                    .await;

                old_state.update(ReceiveSMState::Aborted(error))
            }
        }
    }
}

impl StateMachine for ReceiveStateMachine {
    const DB_PREFIX: u8 = crate::db::DbKeyPrefix::ReceiveStateMachine as u8;

    type Context = WalletClientContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            ReceiveSMState::Funding => {
                let ctx = ctx.clone();
                let txid = self.common.txid;
                vec![SmStateTransition::new(
                    await_funding_sm(ctx.clone(), txid),
                    move |dbtx, result, old_state| {
                        let ctx = ctx.clone();
                        Box::pin(transition_funding_sm(ctx, dbtx, result, old_state))
                    },
                )]
            }
            ReceiveSMState::Success | ReceiveSMState::Aborted(_) => vec![],
        }
    }
}

async fn await_funding_sm(ctx: WalletClientContext, txid: TransactionId) -> Result<(), String> {
    ctx.client_ctx.await_tx_accepted(txid).await
}

async fn transition_funding_sm(
    ctx: WalletClientContext,
    dbtx: &mut DatabaseTransaction<'_>,
    result: Result<(), String>,
    old_state: ReceiveStateMachine,
) -> ReceiveStateMachine {
    match result {
        Ok(()) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    ReceivePaymentUpdateEvent {
                        operation_id: old_state.common.operation_id,
                        status: ReceivePaymentStatus::Success,
                    },
                )
                .await;

            old_state.update(ReceiveSMState::Success)
        }
        Err(error) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    ReceivePaymentUpdateEvent {
                        operation_id: old_state.common.operation_id,
                        status: ReceivePaymentStatus::Aborted,
                    },
                )
                .await;

            old_state.update(ReceiveSMState::Aborted(error))
        }
    }
}
