use std::collections::BTreeMap;

use anyhow::ensure;
use fedimint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use fedimint_client_module::module::OutPointRange;
use fedimint_core::PeerId;
use fedimint_core::core::OperationId;
use fedimint_core::db::{IWriteDatabaseTransactionOpsTyped, WriteDatabaseTransaction};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_mintv2_common::{Denomination, verify_note};
use tbs::{BlindedSignatureShare, PublicKeyShare, aggregate_signature_shares};

use crate::api::MintV2ModuleApi;
use crate::client_db::SpendableNoteKey;
use crate::events::OutputFinalisedEvent;
use crate::{MintSmContext, NoteIssuanceRequest};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct MintOutputStateMachine {
    pub common: OutputSMCommon,
    pub state: OutputSMState,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct OutputSMCommon {
    pub operation_id: OperationId,
    pub range: Option<OutPointRange>,
    pub issuance_requests: Vec<NoteIssuanceRequest>,
}

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub enum OutputSMState {
    /// Issuance request was created, we are waiting for blind signatures.
    Pending,
    /// The transaction containing the issuance was rejected, we can stop
    /// looking for decryption shares.
    Aborted,
    /// The transaction containing the issuance was accepted but an unexpected
    /// error occurred, this should never happen with a honest federation and
    /// bug-free code.
    Failure,
    /// The issuance was completed successfully and the e-cash notes added to
    /// our wallet.
    Success,
}

impl StateMachine for MintOutputStateMachine {
    const DB_PREFIX: u8 = crate::client_db::DbKeyPrefix::OutputStateMachine as u8;

    type Context = MintSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        match &self.state {
            OutputSMState::Pending => {
                let ctx = ctx.clone();
                let range = self.common.range;
                let issuance_requests = self.common.issuance_requests.clone();
                vec![SmStateTransition::new(
                    await_signature_shares_sm(ctx.clone(), range, issuance_requests),
                    move |dbtx, shares, old_state| {
                        let ctx = ctx.clone();
                        let balance_update_sender = ctx.balance_update_sender.clone();
                        dbtx.on_commit(move || {
                            balance_update_sender.send_replace(());
                        });
                        Box::pin(transition_outcome_ready_sm(ctx, dbtx, shares, old_state))
                    },
                )]
            }
            OutputSMState::Aborted | OutputSMState::Failure | OutputSMState::Success => vec![],
        }
    }
}

async fn await_signature_shares_sm(
    ctx: MintSmContext,
    range: Option<OutPointRange>,
    issuance_requests: Vec<NoteIssuanceRequest>,
) -> Result<std::collections::BTreeMap<PeerId, Vec<BlindedSignatureShare>>, String> {
    let tbs_pks = ctx.tbs_pks.clone();
    if let Some(range) = range {
        ctx.client_ctx.await_tx_accepted(range.txid).await?;
        Ok(ctx
            .client_ctx
            .module_api()
            .fetch_signature_shares(range, issuance_requests, tbs_pks)
            .await)
    } else {
        Ok(ctx
            .client_ctx
            .module_api()
            .fetch_signature_shares_recovery(issuance_requests, tbs_pks)
            .await)
    }
}

async fn transition_outcome_ready_sm(
    ctx: MintSmContext,
    dbtx: &mut WriteDatabaseTransaction<'_>,
    signature_shares: Result<
        std::collections::BTreeMap<PeerId, Vec<BlindedSignatureShare>>,
        String,
    >,
    old_state: MintOutputStateMachine,
) -> MintOutputStateMachine {
    let Ok(signature_shares) = signature_shares else {
        return MintOutputStateMachine {
            common: old_state.common,
            state: OutputSMState::Aborted,
        };
    };

    for (i, request) in old_state.common.issuance_requests.iter().enumerate() {
        let agg_blind_signature = aggregate_signature_shares(
            &signature_shares
                .iter()
                .map(|(peer, shares)| (peer.to_usize() as u64, shares[i]))
                .collect(),
        );

        let spendable_note = request.finalize(agg_blind_signature);

        let pk = *ctx
            .tbs_agg_pks
            .get(&request.denomination)
            .expect("No aggregated pk found for denomination");

        if !verify_note(spendable_note.note(), pk) {
            return MintOutputStateMachine {
                common: old_state.common,
                state: OutputSMState::Failure,
            };
        }

        dbtx.insert_new_entry(&SpendableNoteKey(spendable_note), &())
            .await;
    }

    if let Some(range) = old_state.common.range {
        ctx.client_ctx
            .log_event(
                dbtx,
                OutputFinalisedEvent {
                    operation_id: old_state.common.operation_id,
                    range,
                },
            )
            .await;
    }

    MintOutputStateMachine {
        common: old_state.common,
        state: OutputSMState::Success,
    }
}

pub fn verify_blind_shares(
    peer: PeerId,
    signature_shares: Vec<BlindedSignatureShare>,
    issuance_requests: &[NoteIssuanceRequest],
    tbs_pks: &BTreeMap<Denomination, BTreeMap<PeerId, PublicKeyShare>>,
) -> anyhow::Result<Vec<BlindedSignatureShare>> {
    ensure!(
        signature_shares.len() == issuance_requests.len(),
        "Invalid number of signatures shares"
    );

    for (request, share) in issuance_requests.iter().zip(signature_shares.iter()) {
        let amount_key = tbs_pks
            .get(&request.denomination)
            .expect("No pk shares found for denomination")
            .get(&peer)
            .expect("No pk share found for peer");

        ensure!(
            tbs::verify_signature_share(request.blinded_message(), *share, *amount_key),
            "Invalid blind signature"
        );
    }

    Ok(signature_shares)
}
