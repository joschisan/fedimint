use std::collections::BTreeMap;
use std::time::Duration;

use bitcoin_hashes::sha256;
use crate::api::{FederationApi, ServerError};
use crate::query::FilterMapThreshold;
use picomint_core::module::ApiRequestErased;
use picomint_core::{NumPeersExt, PeerId, TransactionId};
use picomint_core::mint::endpoint_constants::{
    RECOVERY_COUNT_ENDPOINT, RECOVERY_SLICE_ENDPOINT, RECOVERY_SLICE_HASH_ENDPOINT,
    SIGNATURE_SHARES_ENDPOINT, SIGNATURE_SHARES_RECOVERY_ENDPOINT,
};
use picomint_core::mint::{Denomination, RecoveryItem};
use tbs::{BlindedMessage, BlindedSignatureShare, PublicKeyShare};

use super::issuance_sm::verify_blind_shares;
use super::NoteIssuanceRequest;

impl FederationApi {
    pub async fn signature_shares(
        &self,
        txid: TransactionId,
        issuance_requests: Vec<NoteIssuanceRequest>,
        tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, PublicKeyShare>>,
    ) -> BTreeMap<PeerId, Vec<BlindedSignatureShare>> {
        self.request_with_strategy_retry(
            // This query collects a threshold of 2f + 1 valid blind signature shares
            FilterMapThreshold::new(
                move |peer, signature_shares| {
                    verify_blind_shares(peer, signature_shares, &issuance_requests, &tbs_pks)
                        .map_err(ServerError::InvalidResponse)
                },
                self.all_peers().to_num_peers(),
            ),
            SIGNATURE_SHARES_ENDPOINT.to_owned(),
            ApiRequestErased::new(txid),
        )
        .await
    }

    pub async fn signature_shares_recovery(
        &self,
        issuance_requests: Vec<NoteIssuanceRequest>,
        tbs_pks: BTreeMap<Denomination, BTreeMap<PeerId, PublicKeyShare>>,
    ) -> BTreeMap<PeerId, Vec<BlindedSignatureShare>> {
        let blinded_messages: Vec<BlindedMessage> = issuance_requests
            .iter()
            .map(NoteIssuanceRequest::blinded_message)
            .collect();

        self.request_with_strategy_retry(
            // This query collects a threshold of 2f + 1 valid blind signature shares
            FilterMapThreshold::new(
                move |peer, signature_shares| {
                    verify_blind_shares(peer, signature_shares, &issuance_requests, &tbs_pks)
                        .map_err(ServerError::InvalidResponse)
                },
                self.all_peers().to_num_peers(),
            ),
            SIGNATURE_SHARES_RECOVERY_ENDPOINT.to_owned(),
            ApiRequestErased::new(blinded_messages),
        )
        .await
    }

    pub async fn recovery_count(&self) -> anyhow::Result<u64> {
        self.request_current_consensus::<u64>(
            RECOVERY_COUNT_ENDPOINT.to_string(),
            ApiRequestErased::default(),
        )
        .await
        .map_err(|e| anyhow::anyhow!("{}", e))
    }

    pub async fn recovery_slice_hash(&self, start: u64, end: u64) -> sha256::Hash {
        self.request_current_consensus_retry(
            RECOVERY_SLICE_HASH_ENDPOINT.to_owned(),
            ApiRequestErased::new((start, end)),
        )
        .await
    }

    pub async fn recovery_slice(
        &self,
        peer: PeerId,
        timeout: Duration,
        start: u64,
        end: u64,
    ) -> anyhow::Result<Vec<RecoveryItem>> {
        let result = tokio::time::timeout(
            timeout,
            self.request_single_peer::<Vec<RecoveryItem>>(
                RECOVERY_SLICE_ENDPOINT.to_owned(),
                ApiRequestErased::new((start, end)),
                peer,
            ),
        )
        .await??;

        Ok(result)
    }
}
