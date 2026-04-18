use crate::api::{FederationApi, FederationResult};
use picomint_core::OutPoint;
use picomint_core::module::ApiRequestErased;
use picomint_core::ln::ContractId;
use picomint_core::ln::endpoint_constants::OUTGOING_CONTRACT_EXPIRATION_ENDPOINT;

#[async_trait::async_trait]
pub trait GatewayFederationApi {
    async fn outgoing_contract_expiration(
        &self,
        outpoint: OutPoint,
    ) -> FederationResult<Option<(ContractId, u64)>>;
}

#[async_trait::async_trait]
impl GatewayFederationApi for FederationApi {
    async fn outgoing_contract_expiration(
        &self,
        outpoint: OutPoint,
    ) -> FederationResult<Option<(ContractId, u64)>> {
        self.request_current_consensus(
            OUTGOING_CONTRACT_EXPIRATION_ENDPOINT.to_string(),
            ApiRequestErased::new(outpoint),
        )
        .await
    }
}
