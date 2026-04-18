use std::collections::BTreeMap;

use anyhow::anyhow;
use picomint_api_client::api::ServerError;
use picomint_api_client::query::FilterMapThreshold;
use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_client_module::transaction::{ClientInput, ClientInputBundle};
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::module::ApiRequestErased;
use picomint_core::secp256k1::Keypair;
use picomint_core::{NumPeersExt, OutPoint, PeerId};
use picomint_ln_common::LightningInput;
use picomint_ln_common::contracts::IncomingContract;
use picomint_ln_common::endpoint_constants::DECRYPTION_KEY_SHARE_ENDPOINT;
use picomint_logging::LOG_CLIENT_MODULE_GW;
use picomint_redb::WriteTxRef;
use tpe::{DecryptionKeyShare, aggregate_dk_shares};
use tracing::warn;

use super::events::{ReceivePaymentStatus, ReceivePaymentUpdateEvent};
use crate::GwV2SmContext;

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
    pub outpoint: OutPoint,
    pub refund_keypair: Keypair,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum ReceiveSMState {
    Funding,
}

#[cfg_attr(doc, aquamarine::aquamarine)]
/// State machine that handles the relay of an incoming Lightning payment.
/// Terminates once decryption shares are either invalid, produce a valid
/// preimage (success), or fail to decode one (refunded).
impl StateMachine for ReceiveStateMachine {
    const TABLE_NAME: &'static str = "receive-sm";

    type Context = GwV2SmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx_clone = ctx.clone();
        let operation_id = self.common.operation_id;
        let outpoint = self.common.outpoint;
        let contract = self.common.contract.clone();
        vec![SmStateTransition::new(
            await_decryption_shares_sm(ctx_clone.clone(), operation_id, outpoint, contract),
            move |dbtx, shares, old_state| {
                let ctx = ctx_clone.clone();
                Box::pin(transition_decryption_shares_sm(
                    ctx, dbtx, old_state, shares,
                ))
            },
        )]
    }
}

async fn await_decryption_shares_sm(
    ctx: GwV2SmContext,
    operation_id: OperationId,
    outpoint: OutPoint,
    contract: IncomingContract,
) -> Result<BTreeMap<PeerId, DecryptionKeyShare>, String> {
    ctx.client_ctx
        .await_tx_accepted(operation_id, outpoint.txid)
        .await?;

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
                        return Err(picomint_api_client::api::ServerError::InvalidResponse(
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
    dbtx: &WriteTxRef<'_>,
    old_state: ReceiveStateMachine,
    decryption_shares: Result<BTreeMap<PeerId, DecryptionKeyShare>, String>,
) -> Option<ReceiveStateMachine> {
    let decryption_shares = match decryption_shares {
        Ok(shares) => shares
            .into_iter()
            .map(|(peer, share)| (peer.to_usize() as u64, share))
            .collect(),
        Err(_) => {
            ctx.client_ctx
                .log_event(
                    dbtx,
                    old_state.common.operation_id,
                    ReceivePaymentUpdateEvent {
                        status: ReceivePaymentStatus::Rejected,
                    },
                )
                .await;

            return None;
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
                old_state.common.operation_id,
                ReceivePaymentUpdateEvent {
                    status: ReceivePaymentStatus::Failure,
                },
            )
            .await;

        return None;
    }

    if let Some(preimage) = old_state
        .common
        .contract
        .decrypt_preimage(&agg_decryption_key)
    {
        ctx.client_ctx
            .log_event(
                dbtx,
                old_state.common.operation_id,
                ReceivePaymentUpdateEvent {
                    status: ReceivePaymentStatus::Success(preimage),
                },
            )
            .await;

        return None;
    }

    let client_input = ClientInput::<LightningInput> {
        input: LightningInput::Incoming(old_state.common.outpoint, agg_decryption_key),
        amount: old_state.common.contract.commitment.amount,
        keys: vec![old_state.common.refund_keypair],
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
            ReceivePaymentUpdateEvent {
                status: ReceivePaymentStatus::Refunded,
            },
        )
        .await;

    None
}
