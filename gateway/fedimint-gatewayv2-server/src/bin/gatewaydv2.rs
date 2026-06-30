#![warn(missing_docs)]
//! This crate provides `gatewaydv2`, a Fedimint gateway binary scoped to LDK
//! and LNv2.
//!
//! It started as a copy of the `gatewayd` binary and its
//! `fedimint-gateway-server` crate. LND and LNv1 support are being stripped out
//! so that `gatewaydv2` only supports the LDK lightning backend and the LNv2
//! module.
//!
//! Like `gatewayd`, it runs a webserver with a REST API that Fedimint clients
//! use to request routing of payments through the Lightning Network, plus
//! endpoints for managing the gateway.

use std::sync::Arc;

use fedimint_core::fedimint_build_code_version_env;
use fedimint_core::util::handle_version_hash_command;
use fedimint_gatewayv2_server::Gateway;
use fedimint_logging::{LOG_GATEWAY, TracingSetup};
#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;
use tracing::info;

#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
#[global_allocator]
// rocksdb suffers from memory fragmentation when using standard allocator
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> Result<(), anyhow::Error> {
    let runtime = Arc::new(tokio::runtime::Runtime::new()?);
    runtime.block_on(async {
        handle_version_hash_command(fedimint_build_code_version_env!());
        TracingSetup::default().init()?;
        let gatewayd = Gateway::new_with_default_modules().await?;
        let shutdown_receiver = gatewayd.clone().run(runtime.clone()).await?;
        shutdown_receiver.await;
        info!(target: LOG_GATEWAY, "Gatewaydv2 exiting...");
        Ok(())
    })
}
