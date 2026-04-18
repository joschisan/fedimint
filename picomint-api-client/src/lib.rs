#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]

use std::collections::BTreeMap;

use anyhow::bail;
use api::{FederationApi, ServerError};
use config::ConsensusConfig;
use picomint_core::PeerId;
use picomint_core::endpoint_constants::CLIENT_CONFIG_ENDPOINT;
use picomint_core::invite_code::InviteCode;
use picomint_core::module::ApiRequestErased;
use picomint_logging::LOG_CLIENT_NET;
use query::FilterMap;
use tracing::debug;

pub mod api;
/// Federation-wide consensus config (shared between client and server).
pub mod config;
/// Client query system
pub mod query;
/// Consensus session outcome types (AcceptedItem, SessionOutcome, …).
pub mod session_outcome;
/// Wire-level Transaction and ConsensusItem types.
pub mod transaction;
/// Static wire enums over the fixed module set.
pub mod wire;

pub use iroh::Endpoint;

/// Downloads the [`ConsensusConfig`] using the peers advertised in the invite
/// code, then re-verifies it with the full peer set from the config itself.
pub async fn download_from_invite_code(
    endpoint: &Endpoint,
    invite: &InviteCode,
) -> anyhow::Result<(ConsensusConfig, FederationApi)> {
    debug!(
        target: LOG_CLIENT_NET,
        %invite,
        peers = ?invite.peers(),
        "Downloading client config via invite code"
    );

    let federation_id = invite.federation_id();
    let api_from_invite = FederationApi::new(endpoint.clone(), invite.peers());

    let query_strategy = FilterMap::new(move |cfg: ConsensusConfig| {
        if federation_id != cfg.calculate_federation_id() {
            return Err(ServerError::ConditionFailed(anyhow::anyhow!(
                "FederationId in invite code does not match client config"
            )));
        }

        Ok(cfg.iroh_endpoints.clone())
    });

    let api_endpoints: BTreeMap<PeerId, picomint_core::config::PeerEndpoint> = api_from_invite
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
        .request_current_consensus::<ConsensusConfig>(
            CLIENT_CONFIG_ENDPOINT.to_owned(),
            ApiRequestErased::default(),
        )
        .await?;

    if client_config.calculate_federation_id() != federation_id {
        bail!("Obtained client config has different federation id");
    }

    Ok((client_config, api_full))
}
