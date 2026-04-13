use fedimint_api_client::api::{FederationApiExt, FederationResult, IModuleFederationApi};
use fedimint_core::module::ApiRequestErased;
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::{OutPoint, apply, async_trait_maybe_send};
use fedimint_walletv2_common::endpoint_constants::{
    CONSENSUS_BLOCK_COUNT_ENDPOINT, CONSENSUS_FEERATE_ENDPOINT, FEDERATION_WALLET_ENDPOINT,
    OUTPUT_INFO_SLICE_ENDPOINT, PENDING_TRANSACTION_CHAIN_ENDPOINT, RECEIVE_FEE_ENDPOINT,
    SEND_FEE_ENDPOINT, TRANSACTION_ID_ENDPOINT,
};
use fedimint_walletv2_common::{FederationWallet, OutputInfo, TxInfo};

#[apply(async_trait_maybe_send!)]
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

#[apply(async_trait_maybe_send!)]
impl<T: ?Sized> WalletFederationApi for T
where
    T: IModuleFederationApi + MaybeSend + MaybeSync + 'static,
{
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
