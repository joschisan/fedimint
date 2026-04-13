pub mod p2p;
pub mod p2p_connection;
pub mod p2p_connector;

use async_trait::async_trait;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::module::{ApiEndpointContext, ApiRequestErased};

/// Has the context necessary for serving API endpoints
///
/// Returns the specific `State` the endpoint requires and the
/// `ApiEndpointContext` which all endpoints can access.
#[async_trait]
pub trait HasApiContext<State> {
    async fn context(
        &self,
        request: &ApiRequestErased,
        id: Option<ModuleInstanceId>,
    ) -> (&State, ApiEndpointContext);
}
