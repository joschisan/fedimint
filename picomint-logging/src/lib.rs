#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]

//! Constants for categorizing the logging type
//!
//! To help stabilize logging targets, avoid typos and improve consistency,
//! it's preferable for logging statements use static target constants,
//! that we define in this module.
//!
//! Core + server side components should use global namespace,
//! while client should generally be prefixed with `client::`.
//! This makes it easier to filter interesting calls when
//! running e.g. `devimint`, that will run both server and client
//! side.

use std::fs::File;
use std::{env, io};

use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

pub const LOG_CONSENSUS: &str = "fm::consensus";
pub const LOG_CORE: &str = "fm::core";
pub const LOG_SERVER: &str = "fm::server";
pub const LOG_DB: &str = "fm::db";
pub const LOG_DEVIMINT: &str = "fm::devimint";
pub const LOG_NET: &str = "fm::net";
pub const LOG_NET_IROH: &str = "fm::net::iroh";
pub const LOG_NET_WS: &str = "fm::net::ws";
pub const LOG_NET_API: &str = "fm::net::api";
pub const LOG_NET_PEER_DKG: &str = "fm::net::peer::dkg";
pub const LOG_NET_PEER: &str = "fm::net::peer";
pub const LOG_NET_AUTH: &str = "fm::net::auth";
pub const LOG_TASK: &str = "fm::task";
pub const LOG_RUNTIME: &str = "fm::runtime";
pub const LOG_TEST: &str = "fm::test";
pub const LOG_TIMING: &str = "fm::timing";
pub const LOG_CLIENT: &str = "fm::client";
pub const LOG_CLIENT_DB: &str = "fm::client::db";
pub const LOG_CLIENT_EVENT_LOG: &str = "fm::client::event-log";
pub const LOG_MODULE_MINT: &str = "fm::module::mint";
pub const LOG_MODULE_META: &str = "fm::module::meta";
pub const LOG_MODULE_WALLET: &str = "fm::module::wallet";
pub const LOG_MODULE_LN: &str = "fm::module::ln";
pub const LOG_CLIENT_REACTOR: &str = "fm::client::reactor";
pub const LOG_CLIENT_NET: &str = "fm::client::net";
pub const LOG_CLIENT_NET_API: &str = "fm::client::net::api";
pub const LOG_CLIENT_BACKUP: &str = "fm::client::backup";
pub const LOG_CLIENT_RECOVERY: &str = "fm::client::recovery";
pub const LOG_CLIENT_RECOVERY_MINT: &str = "fm::client::recovery::mint";
pub const LOG_CLIENT_MODULE_MINT: &str = "fm::client::module::mint";
pub const LOG_CLIENT_MODULE_META: &str = "fm::client::module::meta";
pub const LOG_CLIENT_MODULE_LN: &str = "fm::client::module::ln";
pub const LOG_CLIENT_MODULE_WALLET: &str = "fm::client::module::wallet";
pub const LOG_CLIENT_MODULE_GW: &str = "fm::client::module::gw";
pub const LOG_GATEWAY: &str = "fm::gw";
pub const LOG_GATEWAY_UI: &str = "fm::gw::ui";
pub const LOG_LIGHTNING: &str = "fm::gw::lightning";
pub const LOG_BITCOIND_ESPLORA: &str = "fm::bitcoind::esplora";
pub const LOG_BITCOIND_CORE: &str = "fm::bitcoind::bitcoincore";
pub const LOG_BITCOIND: &str = "fm::bitcoind";
pub const LOG_BITCOIN: &str = "fm::bitcoin";

/// Consolidates the setup of server tracing into a helper
#[derive(Default)]
pub struct TracingSetup {
    base_level: Option<String>,
    extra_directives: Option<String>,
    with_file: Option<File>,
}

impl TracingSetup {
    pub fn with_file(&mut self, file: Option<File>) -> &mut Self {
        self.with_file = file;
        self
    }

    /// Sets the log level applied to most modules. Some overly chatty modules
    /// are muted even if this is set to a lower log level, use the `RUST_LOG`
    /// environment variable to override.
    pub fn with_base_level(&mut self, level: impl Into<String>) -> &mut Self {
        self.base_level = Some(level.into());
        self
    }

    /// Add a filter directive.
    pub fn with_directive(&mut self, directive: &str) -> &mut Self {
        if let Some(old) = self.extra_directives.as_mut() {
            *old = format!("{old},{directive}");
        } else {
            self.extra_directives = Some(directive.to_owned());
        }
        self
    }

    /// Initialize the logging, must be called for tracing to begin
    pub fn init(&mut self) -> anyhow::Result<()> {
        use tracing_subscriber::fmt::writer::{BoxMakeWriter, Tee};

        let var = env::var(tracing_subscriber::EnvFilter::DEFAULT_ENV).unwrap_or_default();
        let filter_layer = EnvFilter::builder().parse(format!(
            // We prefix everything with a default general log level and
            // good per-module specific default. User provided RUST_LOG
            // can override one or both
            "{},{},{},{},{},{},{},{},{}",
            self.base_level.as_deref().unwrap_or("info"),
            "jsonrpsee_core::client::async_client=off",
            "hyper=off",
            "h2=off",
            "jsonrpsee_server=warn,jsonrpsee_server::transport=off",
            "AlephBFT-=error",
            "iroh=error",
            var,
            self.extra_directives.as_deref().unwrap_or(""),
        ))?;

        let fmt_writer = match self.with_file.take() {
            Some(file) => BoxMakeWriter::new(Tee::new(io::stderr, file)),
            _ => BoxMakeWriter::new(io::stderr),
        };

        let fmt_layer = tracing_subscriber::fmt::layer()
            .with_thread_names(false) // can be enabled for debugging
            .with_writer(fmt_writer);

        tracing_subscriber::registry()
            .with(filter_layer)
            .with(fmt_layer)
            .try_init()?;
        Ok(())
    }
}
