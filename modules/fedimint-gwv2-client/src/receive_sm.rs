use core::fmt;
use std::collections::BTreeMap;

use anyhow::anyhow;
use fedimint_api_client::api::{FederationApiExt, ServerError};
use fedimint_api_client::query::FilterMapThreshold;
use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::transaction::{ClientInput, ClientInputBundle};
use fedimint_core::core::OperationId;
use fedimint_core::db::WriteDatabaseTransaction;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::ApiRequestErased;
use fedimint_core::secp256k1::Keypair;
use fedimint_core::{NumPeersExt, OutPoint, PeerId};
use fedimint_lnv2_common::contracts::IncomingContract;
use fedimint_lnv2_common::endpoint_constants::DECRYPTION_KEY_SHARE_ENDPOINT;
use fedimint_lnv2_common::{LightningInput, LightningInputV0};
use fedimint_logging::LOG_CLIENT_MODULE_GW;
use tpe::{DecryptionKeyShare, aggregate_dk_shares};
use tracing::warn;

use super::events::{ReceivePaymentStatus, ReceivePaymentUpdateEvent};
use crate::GwV2SmContext;

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
impl StateMachine for ReceiveStateMachine {
    const DB_PREFIX: u8 = crate::db::DbKeyPrefix::ReceiveStateMachine as u8;

    type Context = GwV2SmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            ReceiveSMState::Funding => {
                let ctx_clone = ctx.clone();
                let outpoint = self.common.outpoint;
                let contract = self.common.contract.clone();
                vec![SmStateTransition::new(
                    await_decryption_shares_sm(ctx_clone.clone(), outpoint, contract),
                    move |dbtx, shares, old_state| {
                        let ctx = ctx_clone.clone();
                        Box::pin(transition_decryption_shares_sm(
                            ctx, dbtx, old_state, shares,
                        ))
                    },
                )]
            }
            ReceiveSMState::Success(..)
            | ReceiveSMState::Rejected(..)
            | ReceiveSMState::Refunding(..)
            | ReceiveSMState::Failure => vec![],
        }
    }
}

async fn await_decryption_shares_sm(
    ctx: GwV2SmContext,
    outpoint: OutPoint,
    contract: IncomingContract,
) -> Result<BTreeMap<PeerId, DecryptionKeyShare>, String> {
    ctx.client_ctx.await_tx_accepted(outpoint.txid).await?;

    let tpe_pks = ctx.tpe_pks.clone();
    Ok(ctx
        .client_ctx
        .module_api()
        .request_with_strategy_retry(
            FilterMapThreshold::new(
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

                    Ok(share)
                },
                ctx.client_ctx.global_api().all_peers().to_num_peers(),
            ),
            DECRYPTION_KEY_SHARE_ENDPOINT.to_owned(),
            ApiRequestErased::new(outpoint),
        )
        .await)
}

async fn transition_decryption_shares_sm(
    ctx: GwV2SmContext,
    dbtx: &mut WriteDatabaseTransaction<'_>,
    old_state: ReceiveStateMachine,
    decryption_shares: Result<BTreeMap<PeerId, DecryptionKeyShare>, String>,
) -> ReceiveStateMachine {
    let decryption_shares = match decryption_shares {
        Ok(decryption_shares) => decryption_shares
            .into_iter()
            .map(|(peer, share)| (peer.to_usize() as u64, share))
            .collect(),
        Err(error) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    ReceivePaymentUpdateEvent {
                        operation_id: old_state.common.operation_id,
                        status: ReceivePaymentStatus::Rejected,
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
        .verify_agg_decryption_key(&ctx.tpe_agg_pk, &agg_decryption_key)
    {
        warn!(target: LOG_CLIENT_MODULE_GW, "Failed to obtain decryption key. Client config's public keys are inconsistent");

        ctx.client_ctx
            .log_event(
                dbtx,
                ReceivePaymentUpdateEvent {
                    operation_id: old_state.common.operation_id,
                    status: ReceivePaymentStatus::Failure,
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
        ctx.client_ctx
            .log_event(
                dbtx,
                ReceivePaymentUpdateEvent {
                    operation_id: old_state.common.operation_id,
                    status: ReceivePaymentStatus::Success(preimage),
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
        amount: old_state.common.contract.commitment.amount,
        keys: vec![old_state.common.refund_keypair],
    };

    let outpoints = ctx
        .client_ctx
        .claim_inputs(
            dbtx,
            ClientInputBundle::new(vec![client_input]),
            old_state.common.operation_id,
        )
        .await
        .expect("Cannot claim input, additional funding needed")
        .into_iter()
        .collect();

    ctx.client_ctx
        .log_event(
            dbtx,
            ReceivePaymentUpdateEvent {
                operation_id: old_state.common.operation_id,
                status: ReceivePaymentStatus::Refunded,
            },
        )
        .await;

    old_state.update(ReceiveSMState::Refunding(outpoints))
}
