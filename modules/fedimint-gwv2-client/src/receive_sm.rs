use core::fmt;
use std::collections::BTreeMap;
use std::time::SystemTime;

use anyhow::anyhow;
use fedimint_api_client::api::{FederationApiExt, ServerError};
use fedimint_api_client::query::FilterMapThreshold;
use fedimint_client_module::DynGlobalClientContext;
use fedimint_client_module::sm::{ClientSMDatabaseTransaction, State, StateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::core::OperationId;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::{Amounts, ApiRequestErased};
use fedimint_core::secp256k1::Keypair;
use fedimint_core::time::now;
use fedimint_core::{NumPeersExt, OutPoint, PeerId};
use fedimint_lnv2_common::contracts::IncomingContract;
use fedimint_lnv2_common::endpoint_constants::DECRYPTION_KEY_SHARE_ENDPOINT;
use fedimint_lnv2_common::{LightningInput, LightningInputV0};
use fedimint_logging::LOG_CLIENT_MODULE_GW;
use tpe::{AggregatePublicKey, DecryptionKeyShare, PublicKeyShare, aggregate_dk_shares};
use tracing::{debug, info, warn};

use super::events::{IncomingPaymentFailed, IncomingPaymentSucceeded};
use crate::GatewayClientContextV2;

/// Milliseconds since `since`, for the receive timing log lines.
fn elapsed_ms(since: SystemTime) -> u128 {
    now().duration_since(since).unwrap_or_default().as_millis()
}

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
    pub outpoint: OutPoint,
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
        let gateway_context_ready = context.clone();

        match &self.state {
            ReceiveSMState::Funding => {
                vec![StateTransition::new(
                    Self::await_decryption_shares(
                        global_context.clone(),
                        context.tpe_pks.clone(),
                        self.common.outpoint,
                        self.common.contract.clone(),
                    ),
                    move |dbtx, output_outcomes, old_state| {
                        Box::pin(Self::transition_decryption_shares(
                            dbtx,
                            output_outcomes,
                            old_state,
                            gc.clone(),
                            tpe_agg_pk,
                            gateway_context_ready.clone(),
                        ))
                    },
                )]
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
    async fn await_decryption_shares(
        global_context: DynGlobalClientContext,
        tpe_pks: BTreeMap<PeerId, PublicKeyShare>,
        outpoint: OutPoint,
        contract: IncomingContract,
    ) -> Result<BTreeMap<PeerId, DecryptionKeyShare>, String> {
        let num_peers = global_context.api().all_peers().to_num_peers();
        let module_api = global_context.module_api();
        let started = now();
        let payment_image = contract.commitment.payment_image.clone();

        info!(
            target: LOG_CLIENT_MODULE_GW,
            ?payment_image,
            %outpoint,
            "Awaiting decryption key shares"
        );

        // The decryption key share endpoint long-polls until the share exists, which
        // happens atomically when the funding transaction is accepted. We can therefore
        // fire the request up front and let it overlap with awaiting transaction
        // acceptance instead of serializing the two, saving a round trip on the happy
        // path. Acceptance is still awaited to detect a rejected funding transaction.
        let decryption_shares = module_api.request_with_strategy_retry(
            FilterMapThreshold::new(
                {
                    let payment_image = payment_image.clone();

                    move |peer_id, share: DecryptionKeyShare| {
                        if !contract.verify_decryption_share(
                            tpe_pks
                                .get(&peer_id)
                                .ok_or(ServerError::InternalClientError(anyhow!(
                                    "Missing TPE PK for peer {peer_id}?!"
                                )))?,
                            &share,
                        ) {
                            return Err(fedimint_api_client::api::ServerError::InvalidResponse(
                                anyhow!("Invalid decryption share"),
                            ));
                        }

                        debug!(
                            target: LOG_CLIENT_MODULE_GW,
                            ?payment_image,
                            %peer_id,
                            elapsed_ms = elapsed_ms(started),
                            "Received decryption key share"
                        );

                        Ok(share)
                    }
                },
                num_peers,
            ),
            DECRYPTION_KEY_SHARE_ENDPOINT.to_owned(),
            ApiRequestErased::new(outpoint),
        );

        let decryption_shares = std::pin::pin!(decryption_shares);
        let tx_accepted = std::pin::pin!(global_context.await_tx_accepted(outpoint.txid));

        let shares = match futures::future::select(decryption_shares, tx_accepted).await {
            futures::future::Either::Left((shares, _)) => shares,
            futures::future::Either::Right((accepted, decryption_shares)) => {
                accepted?;

                info!(
                    target: LOG_CLIENT_MODULE_GW,
                    ?payment_image,
                    elapsed_ms = elapsed_ms(started),
                    "Funding transaction accepted"
                );

                decryption_shares.await
            }
        };

        // The threshold is met by the fastest responders, so the peers missing
        // here are the ones that were lagging or unreachable.
        info!(
            target: LOG_CLIENT_MODULE_GW,
            ?payment_image,
            elapsed_ms = elapsed_ms(started),
            peers = ?shares.keys().collect::<Vec<_>>(),
            "Decryption key shares reached threshold"
        );

        Ok(shares)
    }

    async fn transition_decryption_shares(
        dbtx: &mut ClientSMDatabaseTransaction<'_, '_>,
        decryption_shares: Result<BTreeMap<PeerId, DecryptionKeyShare>, String>,
        old_state: ReceiveStateMachine,
        global_context: DynGlobalClientContext,
        tpe_agg_pk: AggregatePublicKey,
        client_ctx: GatewayClientContextV2,
    ) -> ReceiveStateMachine {
        let decryption_shares = match decryption_shares {
            Ok(decryption_shares) => decryption_shares
                .into_iter()
                .map(|(peer, share)| (peer.to_usize() as u64, share))
                .collect(),
            Err(error) => {
                client_ctx
                    .module
                    .client_ctx
                    .log_event_for_operation(
                        &mut dbtx.module_tx(),
                        old_state.common.operation_id,
                        IncomingPaymentFailed {
                            payment_image: old_state
                                .common
                                .contract
                                .commitment
                                .payment_image
                                .clone(),
                            error: error.clone(),
                        },
                    )
                    .await;

                return old_state.update(ReceiveSMState::Rejected(error));
            }
        };

        let agg_decryption_key = aggregate_dk_shares(&decryption_shares);

        if !old_state
            .common
            .contract
            .verify_agg_decryption_key(&tpe_agg_pk, &agg_decryption_key)
        {
            warn!(target: LOG_CLIENT_MODULE_GW, "Failed to obtain decryption key. Client config's public keys are inconsistent");

            client_ctx
                .module
                .client_ctx
                .log_event_for_operation(
                    &mut dbtx.module_tx(),
                    old_state.common.operation_id,
                    IncomingPaymentFailed {
                        payment_image: old_state.common.contract.commitment.payment_image.clone(),
                        error: "Client config's public keys are inconsistent".to_string(),
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
            info!(
                target: LOG_CLIENT_MODULE_GW,
                payment_image = ?old_state.common.contract.commitment.payment_image,
                "Decrypted preimage"
            );

            client_ctx
                .module
                .client_ctx
                .log_event_for_operation(
                    &mut dbtx.module_tx(),
                    old_state.common.operation_id,
                    IncomingPaymentSucceeded {
                        payment_image: old_state.common.contract.commitment.payment_image.clone(),
                        preimage: Some(preimage),
                    },
                )
                .await;

            return old_state.update(ReceiveSMState::Success(preimage));
        }

        let client_input = ClientInput::<LightningInput> {
            input: LightningInput::V0(LightningInputV0::Incoming(
                old_state.common.outpoint,
                agg_decryption_key,
            )),
            amounts: Amounts::new_bitcoin(old_state.common.contract.commitment.amount),
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
            .log_event_for_operation(
                &mut dbtx.module_tx(),
                old_state.common.operation_id,
                IncomingPaymentFailed {
                    payment_image: old_state.common.contract.commitment.payment_image.clone(),
                    error: "Failed to decrypt preimage".to_string(),
                },
            )
            .await;

        old_state.update(ReceiveSMState::Refunding(outpoints))
    }
}
