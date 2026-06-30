use std::collections::BTreeMap;

use fedimint_client::ClientHandleArc;
use fedimint_core::config::{FederationId, FederationIdPrefix, JsonClientConfig};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::util::Spanned;
use fedimint_gateway_common::{ConnectorType, FederationConfig};
use fedimint_gwv2_client::GatewayClientModuleV2;
use fedimint_lnv2_common::gateway_api::PaymentFee;
use fedimint_logging::LOG_GATEWAY;
use tracing::info;

use crate::AdminResult;
use crate::error::FederationNotConnected;

#[derive(Debug)]
pub struct FederationManager {
    /// Map of `FederationId` -> `Client`. Used for efficient retrieval of the
    /// client while handling incoming HTLCs.
    clients: BTreeMap<FederationId, Spanned<fedimint_client::ClientHandleArc>>,
}

impl FederationManager {
    pub fn new() -> Self {
        Self {
            clients: BTreeMap::new(),
        }
    }

    pub fn add_client(&mut self, client: Spanned<fedimint_client::ClientHandleArc>) {
        let federation_id = client.borrow().with_sync(|c| c.federation_id());
        self.clients.insert(federation_id, client);
    }

    /// Waits for ongoing incoming LNv2 payments to complete before returning.
    pub async fn wait_for_incoming_payments(&self) -> AdminResult<()> {
        for client in self.clients.values() {
            let active_operations = client.value().get_active_operations().await;
            let operation_log = client.value().operation_log();
            for op_id in active_operations {
                let log_entry = operation_log.get_operation(op_id).await;
                if let Some(entry) = log_entry
                    && entry.operation_module_kind() == "lnv2"
                {
                    let lnv2 = client.value().get_first_module::<GatewayClientModuleV2>()?;
                    lnv2.await_completion(op_id).await;
                }
            }
        }

        info!(target: LOG_GATEWAY, "Finished waiting for incoming payments");
        Ok(())
    }

    pub fn get_client_for_federation_id_prefix(
        &self,
        federation_id_prefix: FederationIdPrefix,
    ) -> Option<Spanned<ClientHandleArc>> {
        self.clients.iter().find_map(|(fid, client)| {
            if fid.to_prefix() == federation_id_prefix {
                Some(client.clone())
            } else {
                None
            }
        })
    }

    pub fn client(&self, federation_id: &FederationId) -> Option<&Spanned<ClientHandleArc>> {
        self.clients.get(federation_id)
    }

    pub async fn get_federation_config(
        &self,
        federation_id: FederationId,
    ) -> AdminResult<JsonClientConfig> {
        let client = self
            .clients
            .get(&federation_id)
            .ok_or(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            })?;
        Ok(client
            .borrow()
            .with(|client| client.get_config_json())
            .await)
    }

    pub async fn get_all_federation_configs(&self) -> BTreeMap<FederationId, JsonClientConfig> {
        let mut federations = BTreeMap::new();
        for (federation_id, client) in &self.clients {
            federations.insert(
                *federation_id,
                client
                    .borrow()
                    .with(|client| client.get_config_json())
                    .await,
            );
        }
        federations
    }
}

/// Builds the transient [`FederationConfig`] used only to populate
/// [`FederationInfo`](fedimint_gateway_common::FederationInfo) responses. The
/// gateway no longer persists a per-federation config: fees are global and the
/// `federation_index`/connector fields are vestigial LNv1 concepts, so they are
/// filled with defaults.
pub(crate) fn transient_federation_config(
    invite_code: InviteCode,
    lightning_fee: PaymentFee,
    transaction_fee: PaymentFee,
) -> FederationConfig {
    FederationConfig {
        invite_code,
        federation_index: 0,
        lightning_fee,
        transaction_fee,
        #[allow(deprecated)]
        _connector: ConnectorType::Tcp,
    }
}
