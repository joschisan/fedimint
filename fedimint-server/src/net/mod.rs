pub mod p2p;
pub mod p2p_connection;
pub mod p2p_connector;

use async_trait::async_trait;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::module::ApiRequestErased;

/// Resolves the handler state for an API request (optionally module-scoped).
#[async_trait]
pub trait HasApiContext<State> {
    async fn context(&self, request: &ApiRequestErased, id: Option<ModuleInstanceId>) -> &State;
}
