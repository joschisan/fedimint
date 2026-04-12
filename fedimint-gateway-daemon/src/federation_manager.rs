use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};

use fedimint_client::ClientHandleArc;
use fedimint_core::PeerId;
use fedimint_core::config::{FederationId, FederationIdPrefix, JsonClientConfig};
use fedimint_core::db::{DatabaseTransaction, IDatabaseTransactionOpsCoreTyped, NonCommittable};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::util::{FmtCompactAnyhow as _, Spanned};
use fedimint_gateway_common::FederationInfo;
use fedimint_gwv2_client::GatewayClientModuleV2;
use fedimint_logging::LOG_GATEWAY;
use tracing::{info, warn};

use crate::Result;
use crate::db::FederationConfigKey;
use crate::error::CliError;

/// The first index that the gateway will assign to a federation.
/// Note: This starts at 1 because LNv1 uses the `federation_index` as an SCID.
/// An SCID of 0 is considered invalid by LND's HTLC interceptor.
const INITIAL_INDEX: u64 = 1;

// TODO: Add support for client lookup by payment hash (for LNv2).
#[derive(Debug)]
pub struct FederationManager {
    /// Map of `FederationId` -> `Client`. Used for efficient retrieval of the
    /// client while handling incoming HTLCs.
    clients: BTreeMap<FederationId, Spanned<fedimint_client::ClientHandleArc>>,

    /// Tracker for federation index assignments. When connecting a new
    /// federation, this value is incremented and assigned to the federation
    /// as the `federation_index`
    next_index: AtomicU64,
}

impl FederationManager {
    pub fn new() -> Self {
        Self {
            clients: BTreeMap::new(),
            next_index: AtomicU64::new(INITIAL_INDEX),
        }
    }

    pub fn add_client(&mut self, _index: u64, client: Spanned<fedimint_client::ClientHandleArc>) {
        let federation_id = client.borrow().with_sync(|c| c.federation_id());
        self.clients.insert(federation_id, client);
    }

    /// Waits for ongoing incoming LNv1 and LNv2 payments to complete before
    /// returning.
    pub async fn wait_for_incoming_payments(&self) -> Result<()> {
        for client in self.clients.values() {
            let active_operations = client.value().get_active_operations().await;
            let operation_log = client.value().operation_log();
            for op_id in active_operations {
                let log_entry = operation_log.get_operation(op_id).await;
                if let Some(entry) = log_entry {
                    if entry.operation_module_kind() == "lnv2" {
                        let lnv2 = client.value().get_first_module::<GatewayClientModuleV2>()?;
                        lnv2.await_completion(op_id).await;
                    }
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

    pub async fn federation_name(&self, client: &ClientHandleArc) -> Option<String> {
        let client_config = client.config().await;
        let federation_name = client_config.global.federation_name();
        federation_name.map(String::from)
    }

    pub async fn federation_info_all_federations(
        &self,
        mut dbtx: DatabaseTransaction<'_, NonCommittable>,
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

            let config = dbtx
                .get_value(&FederationConfigKey { id: *federation_id })
                .await;
            if let Some(config) = config {
                federation_infos.push(FederationInfo {
                    federation_id: *federation_id,
                    federation_name: self.federation_name(client.value()).await,
                    balance_msat,
                    config,
                });
            }
        }
        federation_infos
    }

    pub async fn get_federation_config(
        &self,
        federation_id: FederationId,
    ) -> Result<JsonClientConfig> {
        let client = self.clients.get(&federation_id).ok_or_else(|| {
            CliError::bad_request(format!(
                "No federation available for prefix {}",
                federation_id.to_prefix()
            ))
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

    // TODO(tvolk131): Set this value in the constructor.
    pub fn set_next_index(&self, next_index: u64) {
        self.next_index.store(next_index, Ordering::SeqCst);
    }

    pub fn pop_next_index(&self) -> Result<u64> {
        let next_index = self.next_index.fetch_add(1, Ordering::Relaxed);

        // Check for overflow.
        if next_index == INITIAL_INDEX.wrapping_sub(1) {
            return Err(CliError::internal("Federation Index overflow"));
        }

        Ok(next_index)
    }
}
