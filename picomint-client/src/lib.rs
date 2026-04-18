//! Client library
//!
//! Notably previous [`crate`] became [`picomint_client_module`] and the
//! project is gradually moving things, that are irrelevant to the interface
//! between client and client modules.
//!
//! # Client library for picomintd
//!
//! This library provides a client interface to build module clients that can be
//! plugged together into a picomint client that exposes a high-level interface
//! for application authors to integrate with.
//!
//! ## Module Clients
//! Module clients have to at least implement the
//! [`crate::module::ClientModule`] trait and a factory struct
//! implementing [`crate::module::init::ClientModuleInit`]. The
//! `ClientModule` trait defines the module types (tx inputs, outputs, etc.) as
//! well as the module's [state machines](module::sm::State).
//!
//! ### State machines
//! State machines are spawned when starting operations and drive them
//! forward in the background. All module state machines are run by a central
//! [`crate::sm::executor::Executor`]. This means typically starting an
//! operation shall return instantly.
//!
//! For example when doing a deposit the function starting it would immediately
//! return a deposit address and a [`crate::OperationId`]
//! (important concept, highly recommended to read the docs) while spawning a
//! state machine checking the blockchain for incoming bitcoin transactions. The
//! progress of these state machines can then be *observed* using the operation
//! id, but no further user interaction is required to drive them forward.
//!
//! ### State Machine Contexts
//! State machines have access to both a [global
//! context](`DynGlobalClientContext`) as well as to a [module-specific
//! context](crate::module::ClientModule::context).
//!
//! The global context provides access to the federation API and allows to claim
//! module outputs (and transferring the value into the client's wallet), which
//! can be used for refunds.
//!
//! The client-specific context can be used for other purposes, such as
//! supplying config to the state transitions or giving access to other APIs
//! (e.g. LN gateway in case of the lightning module).
//!
//! ### Extension traits
//! The modules themselves can only create inputs and outputs that then have to
//! be combined into transactions by the user and submitted via
//! [`Client::finalize_and_submit_transaction`]. To make this easier most module
//! client implementations contain an extension trait which is implemented for
//! [`Client`] and allows to create the most typical picomint transactions with
//! a single function call.
//!
//! To observe the progress each high level operation function should be
//! accompanied by one returning a stream of high-level operation updates.
//! Internally that stream queries the state machines belonging to the
//! operation to determine the high-level operation state.
//!
//! ### Primary Modules
//! Not all modules have the ability to hold money for long. E.g. the lightning
//! module and its smart contracts are only used to incentivize LN payments, not
//! to hold money. The mint module on the other hand holds e-cash note and can
//! thus be used to fund transactions and to absorb change. Module clients with
//! this ability should implement
//! [`crate::ClientModule::supports_being_primary`] and related
//! methods.
//!
//! For a example of a client module see [the mint client](https://github.com/picomint/picomint/blob/master/modules/picomint-mint-client/src/lib.rs).
//!
//! ## Client
//! The [`Client`] struct is the main entry point for application authors. It is
//! constructed using its builder which can be obtained via [`Client::builder`].
//! The supported module clients have to be chosen at compile time while the
//! actually available ones will be determined by the config loaded at runtime.
//!
//! For a hacky instantiation of a complete client see the [`ng` subcommand of `picomint-cli`](https://github.com/picomint/picomint/blob/55f9d88e17d914b92a7018de677d16e57ed42bf6/picomint-cli/src/ng.rs#L56-L73).

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

pub use client::builder::{ClientBuilder, ClientPreview, LnInit, RootSecret};
pub use client::handle::{ClientHandle, ClientHandleArc};
pub use client::{Client, LnFlavor};
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

impl<M> std::ops::Deref for ClientModuleInstance<'_, M>
{
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
