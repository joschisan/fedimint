use std::collections::BTreeMap;

use fedimint_client::ClientHandleArc;
use fedimint_core::PeerId;
use fedimint_core::config::FederationId;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::util::{FmtCompactAnyhow as _, Spanned};
use fedimint_gateway_common::FederationInfo;
use fedimint_logging::LOG_GATEWAY;
use tracing::warn;

#[derive(Debug)]
pub struct FederationManager {
    clients: BTreeMap<FederationId, Spanned<ClientHandleArc>>,
}

impl FederationManager {
    pub fn new() -> Self {
        Self {
            clients: BTreeMap::new(),
        }
    }

    pub fn add_client(&mut self, client: Spanned<ClientHandleArc>) {
        let federation_id = client.borrow().with_sync(|c| c.federation_id());
        self.clients.insert(federation_id, client);
    }

    pub fn has_federation(&self, federation_id: FederationId) -> bool {
        self.clients.contains_key(&federation_id)
    }

    pub fn client(&self, federation_id: &FederationId) -> Option<&Spanned<ClientHandleArc>> {
        self.clients.get(federation_id)
    }

    pub async fn federation_name(&self, client: &ClientHandleArc) -> Option<String> {
        let client_config = client.config().await;
        client_config.global.federation_name().map(String::from)
    }

    pub async fn federation_info_all_federations(&self) -> Vec<FederationInfo> {
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

            federation_infos.push(FederationInfo {
                federation_id: *federation_id,
                federation_name: self.federation_name(client.value()).await,
                balance_msat,
            });
        }
        federation_infos
    }

    pub async fn get_all_federation_configs(
        &self,
    ) -> BTreeMap<FederationId, fedimint_core::config::JsonClientConfig> {
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
