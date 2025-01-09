use core::fmt;
use std::collections::BTreeMap;
use std::future::pending;

use anyhow::{anyhow, bail};
use fedimint_api_client::api::{deserialize_outcome, FederationApiExt, SerdeOutputOutcome};
use fedimint_api_client::query::FilterMapThreshold;
use fedimint_client::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_client::transaction::{ClientInput, ClientInputBundle};
use fedimint_client::DynGlobalClientContext;
use fedimint_core::core::{Decoder, OperationId};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::endpoint_constants::AWAIT_OUTPUT_OUTCOME_ENDPOINT;
use fedimint_core::module::ApiRequestErased;
use fedimint_core::secp256k1::Keypair;
use fedimint_core::{NumPeersExt, OutPoint, PeerId, TransactionId};
use fedimint_lnv2_common::contracts::IncomingContract;
use fedimint_lnv2_common::{
    LightningInput, LightningInputV0, LightningOutputOutcome, LightningOutputOutcomeV0,
};
use tpe::{aggregate_dk_shares, AggregatePublicKey, DecryptionKeyShare, PublicKeyShare};
use tracing::error;

use super::events::{IncomingPaymentFailed, IncomingPaymentSucceeded};
use crate::gateway_module_v2::GatewayClientContextV2;

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

impl fmt::Display for ReceiveStateMachine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Receive State Machine Operation ID: {:?} State: {}",
            self.common.operation_id, self.state
        )
    }
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct ReceiveSMCommon {
    pub operation_id: OperationId,
    pub contract: IncomingContract,
    pub out_point: OutPoint,
    pub refund_keypair: Keypair,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Funding,
    Rejected(String),
    Success([u8; 32]),
    Failure,
    Refunding(Vec<OutPoint>),
}

impl fmt::Display for ReceiveSMState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReceiveSMState::Funding => write!(f, "Funding"),
            ReceiveSMState::Rejected(_) => write!(f, "Rejected"),
            ReceiveSMState::Success(_) => write!(f, "Success"),
            ReceiveSMState::Failure => write!(f, "Failure"),
            ReceiveSMState::Refunding(_) => write!(f, "Refunding"),
        }
    }
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that handles the relay of an incoming Lightning payment.
///
/// ```mermaid
/// graph LR
/// classDef virtual fill:#fff,stroke-dasharray: 5 5
///
///     Funding -- funding transaction is rejected --> Rejected
///     Funding -- aggregated decryption key is invalid --> Failure
///     Funding -- decrypted preimage is valid --> Success
///     Funding -- decrypted preimage is invalid --> Refunding
/// ```
impl State for ReceiveStateMachine {
    type ModuleContext = GatewayClientContextV2;

    fn transitions(
        &self,
        context: &Self::ModuleContext,
        global_context: &DynGlobalClientContext,
    ) -> Vec<StateTransition<Self>> {
        let gc = global_context.clone();
        let tpe_agg_pk = context.tpe_agg_pk;
        let gateway_context_rejected = context.clone();
        let gateway_context_ready = context.clone();

        match &self.state {
            ReceiveSMState::Funding => {
                vec![
                    StateTransition::new(
                        Self::await_funding_rejected(
                            global_context.clone(),
                            self.common.out_point.txid,
                        ),
                        move |dbtx, error, old_state| {
                            Box::pin(Self::transition_funding_rejected(
                                error,
                                old_state,
                                dbtx,
                                gateway_context_rejected.clone(),
                            ))
                        },
                    ),
                    StateTransition::new(
                        Self::await_outcome_ready(
                            global_context.clone(),
                            context.decoder.clone(),
                            context.tpe_pks.clone(),
                            self.common.out_point,
                            self.common.contract.clone(),
                        ),
                        move |dbtx, output_outcomes, old_state| {
                            Box::pin(Self::transition_outcome_ready(
                                dbtx,
                                output_outcomes,
                                old_state,
                                gc.clone(),
                                tpe_agg_pk,
                                gateway_context_ready.clone(),
                            ))
                        },
                    ),
                ]
            }
            ReceiveSMState::Success(..)
            | ReceiveSMState::Rejected(..)
            | ReceiveSMState::Refunding(..)
            | ReceiveSMState::Failure => {
                vec![]
            }
        }
    }

    fn operation_id(&self) -> OperationId {
        self.common.operation_id
    }
}

impl ReceiveStateMachine {
    async fn await_funding_rejected(
        global_context: DynGlobalClientContext,
        txid: TransactionId,
    ) -> String {
        match global_context.await_tx_accepted(txid).await {
            Ok(()) => pending().await,
            Err(error) => error,
        }
    }

    async fn transition_funding_rejected(
        error: String,
        old_state: ReceiveStateMachine,
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        client_ctx: GatewayClientContextV2,
    ) -> ReceiveStateMachine {
        client_ctx
            .module
            .client_ctx
            .log_event(
                &mut dbtx.module_tx(),
                IncomingPaymentFailed {
                    payment_image: old_state.common.contract.commitment.payment_image.clone(),
                    error: error.clone(),
                },
            )
            .await;
        old_state.update(ReceiveSMState::Rejected(error))
    }

    async fn await_outcome_ready(
        global_context: DynGlobalClientContext,
        module_decoder: Decoder,
        tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
        out_point: OutPoint,
        decryption_contract: IncomingContract,
    ) -> BTreeMap<PeerId, DecryptionKeyShare> {
        global_context
            .api()
            .request_with_strategy_retry(
                FilterMapThreshold::new(
                    move |peer, outcome: SerdeOutputOutcome| {
                        let outcome = deserialize_outcome::<LightningOutputOutcome>(
                            &outcome,
                            &module_decoder,
                        )?;

                        match outcome.ensure_v0_ref()? {
                            LightningOutputOutcomeV0::Incoming(share) => {
                                if !decryption_contract.verify_decryption_share(
                                    tpe_pks.get(&peer).ok_or(anyhow!("Unknown peer pk"))?,
                                    share,
                                ) {
                                    bail!("Invalid decryption share");
                                }

                                Ok(*share)
                            }
                            LightningOutputOutcomeV0::Outgoing => {
                                bail!("Unexpected outcome variant");
                            }
                        }
                    },
                    global_context.api().all_peers().to_num_peers(),
                ),
                AWAIT_OUTPUT_OUTCOME_ENDPOINT.to_owned(),
                ApiRequestErased::new(out_point),
            )
            .await
    }

    async fn transition_outcome_ready(
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        decryption_shares: BTreeMap<PeerId, DecryptionKeyShare>,
        old_state: ReceiveStateMachine,
        global_context: DynGlobalClientContext,
        tpe_agg_pk: AggregatePublicKey,
        client_ctx: GatewayClientContextV2,
    ) -> ReceiveStateMachine {
        let decryption_shares = decryption_shares
            .into_iter()
            .map(|(peer, share)| (peer.to_usize() as u64, share))
            .collect();

        let agg_decryption_key = aggregate_dk_shares(&decryption_shares);

        if !old_state
            .common
            .contract
            .verify_agg_decryption_key(&tpe_agg_pk, &agg_decryption_key)
        {
            let error =
                "Failed to obtain decryption key. Client config's public keys are inconsistent"
                    .to_string();
            error!(error);

            client_ctx
                .module
                .client_ctx
                .log_event(
                    &mut dbtx.module_tx(),
                    IncomingPaymentFailed {
                        payment_image: old_state.common.contract.commitment.payment_image.clone(),
                        error,
                    },
                )
                .await;

            return old_state.update(ReceiveSMState::Failure);
        }

        if let Some(preimage) = old_state
            .common
            .contract
            .decrypt_preimage(&agg_decryption_key)
        {
            client_ctx
                .module
                .client_ctx
                .log_event(
                    &mut dbtx.module_tx(),
                    IncomingPaymentSucceeded {
                        payment_image: old_state.common.contract.commitment.payment_image.clone(),
                    },
                )
                .await;
            return old_state.update(ReceiveSMState::Success(preimage));
        }

        let client_input = ClientInput::<LightningInput> {
            input: LightningInput::V0(LightningInputV0::Incoming(
                old_state.common.contract.contract_id(),
                agg_decryption_key,
            )),
            amount: old_state.common.contract.commitment.amount,
            keys: vec![old_state.common.refund_keypair],
        };

        let outpoints = global_context
            .claim_inputs(
                dbtx,
                // The input of the refund tx is managed by this state machine
                ClientInputBundle::new_no_sm(vec![client_input]),
            )
            .await
            .expect("Cannot claim input, additional funding needed")
            .into_iter()
            .collect();

        client_ctx
            .module
            .client_ctx
            .log_event(
                &mut dbtx.module_tx(),
                IncomingPaymentFailed {
                    payment_image: old_state.common.contract.commitment.payment_image.clone(),
                    error: "Failed to decrypt preimage".to_string(),
                },
            )
            .await;

        old_state.update(ReceiveSMState::Refunding(outpoints))
    }
}
