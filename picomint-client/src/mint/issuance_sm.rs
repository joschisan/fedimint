use std::collections::BTreeMap;

use crate::executor::StateMachine;
use anyhow::ensure;
use picomint_core::core::OperationId;
use picomint_core::mint::{Denomination, verify_note};
use picomint_core::{PeerId, TransactionId};
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::WriteTxRef;
use tbs::{BlindedSignatureShare, PublicKeyShare, aggregate_signature_shares};

use super::client_db::NOTE;
use super::events::{IssuanceComplete, OutputFailureEvent};
use super::{MintSmContext, NoteIssuanceRequest, SpendableNote};

#[derive(Debug, Clone, Eq, PartialEq, Hash, Decodable, Encodable)]
pub struct IssuanceStateMachine {
    pub operation_id: OperationId,
    /// Notes this tx consumed on its input side that originated from our own
    /// wallet db. Restored to `NOTE` on tx rejection.
    pub spendable_notes: Vec<SpendableNote>,
    /// `Some(txid)` for normal operation. `None` for recovery-bootstrapped
    /// state machines, which fetch shares via the recovery endpoint instead.
    pub txid: Option<TransactionId>,
    /// Blinded outputs this tx issues. Finalized into `SpendableNote`s and
    /// inserted into `NOTE` once the federation's blind-signature shares are
    /// aggregated.
    pub issuance_requests: Vec<NoteIssuanceRequest>,
}

picomint_redb::consensus_value!(IssuanceStateMachine);

impl StateMachine for IssuanceStateMachine {
    const TABLE_NAME: &'static str = "issuance-sm";

    type Context = MintSmContext;
    type Outcome = Result<BTreeMap<PeerId, Vec<BlindedSignatureShare>>, String>;

    async fn trigger(&self, ctx: &Self::Context) -> Self::Outcome {
        if let Some(txid) = self.txid {
            ctx.client_ctx
                .await_tx_accepted(self.operation_id, txid)
                .await?;

            let shares = ctx
                .client_ctx
                .module_api()
                .signature_shares(txid, self.issuance_requests.clone(), ctx.tbs_pks.clone())
                .await;

            Ok(shares)
        } else {
            let shares = ctx
                .client_ctx
                .module_api()
                .signature_shares_recovery(self.issuance_requests.clone(), ctx.tbs_pks.clone())
                .await;

            Ok(shares)
        }
    }

    async fn transition(
        &self,
        ctx: &Self::Context,
        dbtx: &WriteTxRef<'_>,
        outcome: Self::Outcome,
    ) -> Option<Self> {
        let Ok(signature_shares) = outcome else {
            for note in &self.spendable_notes {
                dbtx.insert(&NOTE, note, &());
            }

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

        if let Some(txid) = self.txid {
            ctx.client_ctx
                .log_event(dbtx, self.operation_id, IssuanceComplete { txid })
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
