#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]

use std::collections::BTreeMap;

use anyhow::{Context as _, bail};
use api::{FederationApi, ServerError};
use fedimint_core::PeerId;
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::endpoint_constants::CLIENT_CONFIG_ENDPOINT;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::ApiRequestErased;
use fedimint_core::util::backoff_util;
use fedimint_logging::LOG_CLIENT_NET;
use query::FilterMap;
use tracing::debug;

pub mod api;
/// Client query system
pub mod query;
/// Consensus session outcome types (AcceptedItem, SessionOutcome, …).
pub mod session_outcome;
/// Wire-level Transaction and ConsensusItem types.
pub mod transaction;
/// Static wire enums over the fixed module set.
pub mod wire;

pub use iroh::Endpoint;

/// Tries to download the [`ClientConfig`], attempts to retry ten times before
/// giving up.
pub async fn download_from_invite_code(
    endpoint: &Endpoint,
    invite: &InviteCode,
) -> anyhow::Result<(ClientConfig, FederationApi)> {
    debug!(
        target: LOG_CLIENT_NET,
        %invite,
        peers = ?invite.peers(),
        "Downloading client config via invite code"
    );

    let federation_id = invite.federation_id();
    let api_from_invite = FederationApi::new(endpoint.clone(), invite.peers());

    fedimint_core::util::retry(
        "Downloading client config",
        backoff_util::aggressive_backoff(),
        || try_download_client_config(endpoint, &api_from_invite, federation_id),
    )
    .await
    .context("Failed to download client config")
}

/// Tries to download the [`ClientConfig`] only once.
pub async fn try_download_client_config(
    endpoint: &Endpoint,
    api_from_invite: &FederationApi,
    federation_id: FederationId,
) -> anyhow::Result<(ClientConfig, FederationApi)> {
    debug!(target: LOG_CLIENT_NET, "Downloading client config from peer");
    let query_strategy = FilterMap::new(move |cfg: ClientConfig| {
        if federation_id != cfg.global.calculate_federation_id() {
            return Err(ServerError::ConditionFailed(anyhow::anyhow!(
                "FederationId in invite code does not match client config"
            )));
        }

        Ok(cfg.global.api_endpoints)
    });

    let api_endpoints: BTreeMap<PeerId, fedimint_core::config::PeerEndpoint> = api_from_invite
        .request_with_strategy(
            query_strategy,
            CLIENT_CONFIG_ENDPOINT.to_owned(),
            ApiRequestErased::default(),
        )
        .await?;

    let api_endpoints = api_endpoints
        .into_iter()
        .map(|(peer, endpoint)| (peer, endpoint.node_id))
        .collect();

    debug!(target: LOG_CLIENT_NET, "Verifying client config with all peers");

    let api_full = FederationApi::new(endpoint.clone(), api_endpoints);
    let client_config = api_full
        .request_current_consensus::<ClientConfig>(
            CLIENT_CONFIG_ENDPOINT.to_owned(),
            ApiRequestErased::default(),
        )
        .await?;

    if client_config.calculate_federation_id() != federation_id {
        bail!("Obtained client config has different federation id");
    }

    Ok((client_config, api_full))
}
