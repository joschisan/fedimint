use picomint_api_client::api::{FederationApi, FederationResult};
use picomint_core::OutPoint;
use picomint_core::module::ApiRequestErased;
use picomint_wallet_common::endpoint_constants::{
    CONSENSUS_BLOCK_COUNT_ENDPOINT, CONSENSUS_FEERATE_ENDPOINT, FEDERATION_WALLET_ENDPOINT,
    OUTPUT_INFO_SLICE_ENDPOINT, PENDING_TRANSACTION_CHAIN_ENDPOINT, RECEIVE_FEE_ENDPOINT,
    SEND_FEE_ENDPOINT, TRANSACTION_ID_ENDPOINT,
};
use picomint_wallet_common::{FederationWallet, OutputInfo, TxInfo};

#[async_trait::async_trait]
pub trait WalletFederationApi {
    async fn consensus_block_count(&self) -> FederationResult<u64>;

    async fn consensus_feerate(&self) -> FederationResult<Option<u64>>;

    async fn federation_wallet(&self) -> FederationResult<Option<FederationWallet>>;

    async fn send_fee(&self) -> FederationResult<Option<bitcoin::Amount>>;

    async fn receive_fee(&self) -> FederationResult<Option<bitcoin::Amount>>;

    async fn pending_tx_chain(&self) -> FederationResult<Vec<TxInfo>>;

    async fn output_info_slice(
        &self,
        start_index: u64,
        end_index: u64,
    ) -> FederationResult<Vec<OutputInfo>>;

    async fn tx_id(&self, outpoint: OutPoint) -> Option<bitcoin::Txid>;
}

#[async_trait::async_trait]
impl WalletFederationApi for FederationApi {
    async fn consensus_block_count(&self) -> FederationResult<u64> {
        self.request_current_consensus(
            CONSENSUS_BLOCK_COUNT_ENDPOINT.to_string(),
            ApiRequestErased::new(()),
        )
        .await
    }

    async fn consensus_feerate(&self) -> FederationResult<Option<u64>> {
        self.request_current_consensus(
            CONSENSUS_FEERATE_ENDPOINT.to_string(),
            ApiRequestErased::new(()),
        )
        .await
    }

    async fn federation_wallet(&self) -> FederationResult<Option<FederationWallet>> {
        self.request_current_consensus(
            FEDERATION_WALLET_ENDPOINT.to_string(),
            ApiRequestErased::new(()),
        )
        .await
    }

    async fn send_fee(&self) -> FederationResult<Option<bitcoin::Amount>> {
        self.request_current_consensus(SEND_FEE_ENDPOINT.to_string(), ApiRequestErased::new(()))
            .await
    }

    async fn receive_fee(&self) -> FederationResult<Option<bitcoin::Amount>> {
        self.request_current_consensus(RECEIVE_FEE_ENDPOINT.to_string(), ApiRequestErased::new(()))
            .await
    }

    async fn pending_tx_chain(&self) -> FederationResult<Vec<TxInfo>> {
        self.request_current_consensus(
            PENDING_TRANSACTION_CHAIN_ENDPOINT.to_string(),
            ApiRequestErased::new(()),
        )
        .await
    }

    async fn output_info_slice(
        &self,
        start_index: u64,
        end_index: u64,
    ) -> FederationResult<Vec<OutputInfo>> {
        self.request_current_consensus(
            OUTPUT_INFO_SLICE_ENDPOINT.to_string(),
            ApiRequestErased::new((start_index, end_index)),
        )
        .await
    }

    async fn tx_id(&self, outpoint: OutPoint) -> Option<bitcoin::Txid> {
        self.request_current_consensus_retry(
            TRANSACTION_ID_ENDPOINT.to_string(),
            ApiRequestErased::new(outpoint),
        )
        .await
    }
}
