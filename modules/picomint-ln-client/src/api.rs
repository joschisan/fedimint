use std::collections::{BTreeMap, BTreeSet};

use picomint_api_client::api::{FederationApi, FederationResult, ServerResult};
use picomint_api_client::query::FilterMapThreshold;
use picomint_core::module::ApiRequestErased;
use picomint_core::util::SafeUrl;
use picomint_core::{NumPeersExt, OutPoint, PeerId};
use picomint_ln_common::ContractId;
use picomint_ln_common::contracts::IncomingContract;
use picomint_ln_common::endpoint_constants::{
    AWAIT_INCOMING_CONTRACT_ENDPOINT, AWAIT_INCOMING_CONTRACTS_ENDPOINT, AWAIT_PREIMAGE_ENDPOINT,
    CONSENSUS_BLOCK_COUNT_ENDPOINT, GATEWAYS_ENDPOINT,
};
use rand::seq::SliceRandom;

#[async_trait::async_trait]
pub trait LightningFederationApi {
    async fn consensus_block_count(&self) -> FederationResult<u64>;

    async fn await_incoming_contract(
        &self,
        contract_id: &ContractId,
        expiration: u64,
    ) -> Option<OutPoint>;

    async fn await_preimage(&self, contract_id: OutPoint, expiration: u64) -> Option<[u8; 32]>;

    async fn await_incoming_contracts(&self, start: u64, n: u64) -> (Vec<IncomingContract>, u64);

    async fn gateways(&self) -> FederationResult<Vec<SafeUrl>>;

    async fn gateways_from_peer(&self, peer: PeerId) -> ServerResult<Vec<SafeUrl>>;
}

#[async_trait::async_trait]
impl LightningFederationApi for FederationApi {
    async fn consensus_block_count(&self) -> FederationResult<u64> {
        self.request_current_consensus(
            CONSENSUS_BLOCK_COUNT_ENDPOINT.to_string(),
            ApiRequestErased::new(()),
        )
        .await
    }

    async fn await_incoming_contract(
        &self,
        contract_id: &ContractId,
        expiration: u64,
    ) -> Option<OutPoint> {
        self.request_current_consensus_retry::<Option<OutPoint>>(
            AWAIT_INCOMING_CONTRACT_ENDPOINT.to_string(),
            ApiRequestErased::new((contract_id, expiration)),
        )
        .await
    }

    async fn await_preimage(&self, outpoint: OutPoint, expiration: u64) -> Option<[u8; 32]> {
        self.request_current_consensus_retry(
            AWAIT_PREIMAGE_ENDPOINT.to_string(),
            ApiRequestErased::new((outpoint, expiration)),
        )
        .await
    }

    async fn await_incoming_contracts(&self, start: u64, n: u64) -> (Vec<IncomingContract>, u64) {
        self.request_current_consensus_retry(
            AWAIT_INCOMING_CONTRACTS_ENDPOINT.to_string(),
            ApiRequestErased::new((start, n)),
        )
        .await
    }

    async fn gateways(&self) -> FederationResult<Vec<SafeUrl>> {
        let gateways: BTreeMap<PeerId, Vec<SafeUrl>> = self
            .request_with_strategy(
                FilterMapThreshold::new(
                    |_, gateways| Ok(gateways),
                    self.all_peers().to_num_peers(),
                ),
                GATEWAYS_ENDPOINT.to_string(),
                ApiRequestErased::default(),
            )
            .await?;

        let mut union = gateways
            .values()
            .flatten()
            .cloned()
            .collect::<BTreeSet<SafeUrl>>()
            .into_iter()
            .collect::<Vec<SafeUrl>>();

        // Shuffling the gateways ensures that payments are distributed over the
        // gateways evenly.
        union.shuffle(&mut rand::thread_rng());

        union.sort_by_cached_key(|r| {
            gateways
                .values()
                .filter(|response| !response.contains(r))
                .count()
        });

        Ok(union)
    }

    async fn gateways_from_peer(&self, peer: PeerId) -> ServerResult<Vec<SafeUrl>> {
        let gateways = self
            .request_single_peer::<Vec<SafeUrl>>(
                GATEWAYS_ENDPOINT.to_string(),
                ApiRequestErased::default(),
                peer,
            )
            .await?;

        Ok(gateways)
    }
}
