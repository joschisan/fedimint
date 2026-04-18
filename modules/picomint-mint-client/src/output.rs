use std::collections::BTreeMap;

use anyhow::ensure;
use picomint_client_module::executor::StateMachine;
use picomint_client_module::module::OutPointRange;
use picomint_core::PeerId;
use picomint_core::core::OperationId;
use picomint_encoding::{Decodable, Encodable};
use picomint_mint_common::{Denomination, verify_note};
use picomint_redb::WriteTxRef;
use tbs::{BlindedSignatureShare, PublicKeyShare, aggregate_signature_shares};

use crate::api::MintV2ModuleApi;
use crate::client_db::NOTE;
use crate::events::{OutputFailureEvent, OutputFinalEvent};
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
    type Outcome = Result<BTreeMap<PeerId, Vec<BlindedSignatureShare>>, String>;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        let tbs_pks = ctx.tbs_pks.clone();
        let shares = if let Some(range) = self.range {
            ctx.client_ctx
                .await_tx_accepted(self.operation_id, range.txid)
                .await?;
            ctx.client_ctx
                .module_api()
                .fetch_signature_shares(range, self.issuance_requests.clone(), tbs_pks)
                .await
        } else {
            ctx.client_ctx
                .module_api()
                .fetch_signature_shares_recovery(self.issuance_requests.clone(), tbs_pks)
                .await
        };
        Ok(shares)
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        let balance_update_sender = ctx.balance_update_sender.clone();
        dbtx.on_commit(move || {
            balance_update_sender.send_replace(());
        });

        let Ok(signature_shares) = outcome else {
            ctx.client_ctx
                .log_event(dbtx, self.operation_id, OutputFailureEvent)
                .await;
            return None;
        };

        for (i, request) in self.issuance_requests.iter().enumerate() {
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
                ctx.client_ctx
                    .log_event(dbtx, self.operation_id, OutputFailureEvent)
                    .await;
                return None;
            }

            assert!(dbtx.insert(&NOTE, &spendable_note, &()).is_none());
        }

        if let Some(range) = self.range {
            ctx.client_ctx
                .log_event(dbtx, self.operation_id, OutputFinalEvent { range })
                .await;
        }

        None
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
