//! Picomint client library.
//!
//! [`Client`] is the entry point for applications interacting with one
//! federation. Use [`Client::join`] / [`Client::open`] for regular clients
//! and [`Client::join_gateway`] / [`Client::open_gateway`] for the gateway
//! daemon's flavor.
//!
//! Per-module logic lives in [`mod@mint`], [`mod@wallet`], [`mod@ln`], and
//! [`mod@gw`]. Each module owns its own state machines and exposes a
//! `*Module::new` constructor used by the [`Client`] entry points.
//! Submission ownership lives entirely in [`crate::mint::MintClientModule`]
//! — non-mint modules build a [`crate::transaction::TransactionBuilder`]
//! and call `MintClientModule::finalize_and_submit_transaction`, which
//! balances against the wallet and submits via its own
//! [`crate::transaction::TxSubmissionStateMachine`].

/// Federation API transport
pub mod api;
/// Core [`Client`]
mod client;
/// Database keys used by the client
pub mod db;
/// Environment variables
pub mod envs;
/// Per-module typed state machine executor
pub mod executor;
/// Gateway lightning module (mounted by the gateway daemon).
pub mod gw;
/// Lightning module client.
pub mod ln;
/// Mint module client.
pub mod mint;
/// Module client interface definitions
pub mod module;
/// Client query-consensus strategies
pub mod query;
/// Secret handling & derivation
pub mod secret;
/// Structs and interfaces to construct Picomint transactions
pub mod transaction;
/// Wallet module client.
pub mod wallet;

use std::collections::BTreeMap;

use anyhow::bail;
use api::{FederationApi, ServerError};
pub use iroh::Endpoint;
use picomint_core::PeerId;
use picomint_core::config::ConsensusConfig;
use picomint_core::endpoint_constants::CLIENT_CONFIG_ENDPOINT;
use picomint_core::invite_code::InviteCode;
use picomint_core::module::ApiRequestErased;
use picomint_logging::LOG_CLIENT_NET;
use query::FilterMap;
use tracing::debug;

pub use client::handle::{ClientHandle, ClientHandleArc};
pub use client::{Client, LnFlavor, RootSecret};
pub use picomint_core::core::{ModuleKind, OperationId};

use picomint_core::TransactionId;
use picomint_eventlog::{Event, EventKind};
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TxAcceptEvent {
    pub txid: TransactionId,
}

impl Event for TxAcceptEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-accept");
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TxRejectEvent {
    pub txid: TransactionId,
    pub error: String,
}
impl Event for TxRejectEvent {
    const MODULE: Option<ModuleKind> = None;
    const KIND: EventKind = EventKind::from_static("tx-reject");
}

/// Resources particular to a module instance
pub struct ClientModuleInstance<'m, M> {
    /// Module-specific DB
    pub db: picomint_redb::Database,
    /// Module-specific API
    pub api: api::FederationApi,

    pub module: &'m M,
}

impl<'m, M> ClientModuleInstance<'m, M> {
    /// Get a reference to the module
    pub fn inner(&self) -> &'m M {
        self.module
    }
}

impl<M> std::ops::Deref for ClientModuleInstance<'_, M> {
    type Target = M;

    fn deref(&self) -> &Self::Target {
        self.module
    }
}

#[derive(Deserialize)]
pub struct GetInviteCodeRequest {
    pub peer: PeerId,
}

/// Downloads the [`ConsensusConfig`] using the peers advertised in the invite
/// code, then re-verifies it with the full peer set from the config itself.
pub async fn download_from_invite_code(
    endpoint: &Endpoint,
    invite: &InviteCode,
) -> anyhow::Result<ConsensusConfig> {
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

    Ok(client_config)
}
