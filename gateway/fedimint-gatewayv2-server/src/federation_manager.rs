use std::collections::BTreeMap;
use std::sync::Arc;

use fedimint_client::ClientHandleArc;
use fedimint_core::PeerId;
use fedimint_core::config::{FederationId, FederationIdPrefix, JsonClientConfig};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::util::{FmtCompactAnyhow as _, Spanned};
use fedimint_gateway_common::{ConnectorType, FederationConfig, FederationInfo};
use fedimint_gwv2_client::GatewayClientModuleV2;
use fedimint_lnv2_common::gateway_api::PaymentFee;
use fedimint_logging::LOG_GATEWAY;
use tracing::{info, warn};

use crate::AdminResult;
use crate::error::{AdminGatewayError, FederationNotConnected};

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

    pub async fn leave_federation(
        &mut self,
        federation_id: FederationId,
        lightning_fee: PaymentFee,
        transaction_fee: PaymentFee,
    ) -> AdminResult<FederationInfo> {
        let federation_info = self
            .federation_info(federation_id, lightning_fee, transaction_fee)
            .await?;

        self.remove_client(federation_id).await?;

        Ok(federation_info)
    }

    /// Reconstructs an invite code for a connected federation from the live
    /// client (the gateway no longer persists invite codes). Returns the first
    /// peer's invite code, or `None` if none can be derived.
    async fn client_invite_code(client: &ClientHandleArc) -> Option<InviteCode> {
        let config = client.config().await;
        for peer_id in config.global.api_endpoints.keys() {
            if let Some(code) = client.invite_code(*peer_id).await {
                return Some(code);
            }
        }
        None
    }

    async fn remove_client(&mut self, federation_id: FederationId) -> AdminResult<()> {
        let client = self
            .clients
            .remove(&federation_id)
            .ok_or(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            })?
            .into_value();

        match Arc::into_inner(client) {
            Some(client) => {
                client.shutdown().await;
                Ok(())
            }
            _ => Err(AdminGatewayError::ClientRemovalError(format!(
                "Federation client {federation_id} is not unique, failed to shutdown client"
            ))),
        }
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

    pub fn has_federation(&self, federation_id: FederationId) -> bool {
        self.clients.contains_key(&federation_id)
    }

    pub fn client(&self, federation_id: &FederationId) -> Option<&Spanned<ClientHandleArc>> {
        self.clients.get(federation_id)
    }

    pub async fn federation_info(
        &self,
        federation_id: FederationId,
        lightning_fee: PaymentFee,
        transaction_fee: PaymentFee,
    ) -> std::result::Result<FederationInfo, FederationNotConnected> {
        self.clients
            .get(&federation_id)
            .ok_or(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            })?
            .borrow()
            .with(|client| async move {
                let balance_msat = client
                    .get_balance_for_btc()
                    .await
                    // If primary module is not available, we're not really connected yet
                    .map_err(|_err| FederationNotConnected {
                        federation_id_prefix: federation_id.to_prefix(),
                    })?;

                let invite_code =
                    Self::client_invite_code(client)
                        .await
                        .ok_or(FederationNotConnected {
                            federation_id_prefix: federation_id.to_prefix(),
                        })?;

                Ok(FederationInfo {
                    federation_id,
                    federation_name: self.federation_name(client).await,
                    balance_msat,
                    config: transient_federation_config(
                        invite_code,
                        lightning_fee,
                        transaction_fee,
                    ),
                    last_backup_time: None,
                })
            })
            .await
    }

    pub async fn federation_name(&self, client: &ClientHandleArc) -> Option<String> {
        let client_config = client.config().await;
        let federation_name = client_config.global.federation_name();
        federation_name.map(String::from)
    }

    pub async fn federation_info_all_federations(
        &self,
        lightning_fee: PaymentFee,
        transaction_fee: PaymentFee,
    ) -> Vec<FederationInfo> {
        let mut federation_infos = Vec::new();
        for (federation_id, client) in &self.clients {
            let balance_msat = match client
                .borrow()
                .with(|client| client.get_balance_for_btc())
                .await
            {
                Ok(balance_msat) => balance_msat,
                Err(err) => {
                    warn!(
                        target: LOG_GATEWAY,
                        err = %err.fmt_compact_anyhow(),
                        "Skipped Federation due to lack of primary module"
                    );
                    continue;
                }
            };

            let Some(invite_code) = Self::client_invite_code(client.value()).await else {
                warn!(
                    target: LOG_GATEWAY,
                    federation_id = %federation_id,
                    "Skipped Federation; could not derive an invite code"
                );
                continue;
            };

            federation_infos.push(FederationInfo {
                federation_id: *federation_id,
                federation_name: self.federation_name(client.value()).await,
                balance_msat,
                config: transient_federation_config(invite_code, lightning_fee, transaction_fee),
                last_backup_time: None,
            });
        }
        federation_infos
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

    pub async fn all_invite_codes(
        &self,
    ) -> BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>> {
        let mut invite_codes = BTreeMap::new();

        for (federation_id, client) in &self.clients {
            let config = client.value().config().await;
            let api_endpoints = &config.global.api_endpoints;

            let mut fed_invite_codes = BTreeMap::new();
            for (peer_id, peer_url) in api_endpoints {
                if let Some(code) = client.value().invite_code(*peer_id).await {
                    fed_invite_codes.insert(*peer_id, (peer_url.name.clone(), code));
                }
            }

            invite_codes.insert(*federation_id, fed_invite_codes);
        }

        invite_codes
    }
}

/// Builds the transient [`FederationConfig`] used only to populate
/// [`FederationInfo`] responses. The gateway no longer persists a
/// per-federation config: fees are global and the `federation_index`/connector
/// fields are vestigial LNv1 concepts, so they are filled with defaults.
fn transient_federation_config(
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
