use std::collections::BTreeMap;

use anyhow::ensure;
use picomint_client_module::executor::{StateMachine, StateTransition as SmStateTransition};
use picomint_client_module::module::OutPointRange;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_core::PeerId;
use picomint_mint_common::{verify_note, Denomination};
use picomint_redb::WriteTxRef;
use tbs::{aggregate_signature_shares, BlindedSignatureShare, PublicKeyShare};

use crate::api::MintV2ModuleApi;
use crate::client_db::NOTE;
use crate::events::OutputFinalisedEvent;
use crate::{MintSmContext, NoteIssuanceRequest};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct MintOutputStateMachine {
    pub operation_id: OperationId,
    pub range: Option<OutPointRange>,
    pub issuance_requests: Vec<NoteIssuanceRequest>,
}

picomint_redb::consensus_value!(MintOutputStateMachine);

impl StateMachine for MintOutputStateMachine {
    const TABLE_NAME: &'static str = "output-sm";

    type Context = MintSmContext;

    fn transitions(&self, ctx: &Self::Context) -> Vec<SmStateTransition<Self>> {
        let ctx = ctx.clone();
        let operation_id = self.operation_id;
        let range = self.range;
        let issuance_requests = self.issuance_requests.clone();
        vec![SmStateTransition::new(
            await_signature_shares_sm(ctx.clone(), operation_id, range, issuance_requests),
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
}

async fn await_signature_shares_sm(
    ctx: MintSmContext,
    operation_id: OperationId,
    range: Option<OutPointRange>,
    issuance_requests: Vec<NoteIssuanceRequest>,
) -> Result<std::collections::BTreeMap<PeerId, Vec<BlindedSignatureShare>>, String> {
    let tbs_pks = ctx.tbs_pks.clone();
    if let Some(range) = range {
        ctx.client_ctx
            .await_tx_accepted(operation_id, range.txid)
            .await?;
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
    dbtx: &WriteTxRef<'_>,
    signature_shares: Result<
        std::collections::BTreeMap<PeerId, Vec<BlindedSignatureShare>>,
        String,
    >,
    old_state: MintOutputStateMachine,
) -> Option<MintOutputStateMachine> {
    let Ok(signature_shares) = signature_shares else {
        return None;
    };

    for (i, request) in old_state.issuance_requests.iter().enumerate() {
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
            return None;
        }

        assert!(dbtx.insert(&NOTE, &spendable_note, &()).is_none());
    }

    if let Some(range) = old_state.range {
        ctx.client_ctx
            .log_event(
                dbtx,
                old_state.operation_id,
                OutputFinalisedEvent { range },
            )
            .await;
    }

    None
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
