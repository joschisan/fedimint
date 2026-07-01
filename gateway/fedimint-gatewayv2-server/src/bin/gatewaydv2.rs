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
//! use to request routing of payments through the Lightning Network, plus an
//! admin CLI over a local Unix socket. The daemon lifecycle is modeled on the
//! sibling picomint gateway: `main` wires every component and fires each
//! long-running task off onto the runtime, then blocks on `SIGTERM`; the
//! runtime drop on exit aborts the tasks.

use std::sync::Arc;

use clap::Parser;
use fedimint_core::fedimint_build_code_version_env;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::util::handle_version_hash_command;
use fedimint_gatewayv2_server::{
    Gateway, GatewayOpts, LDK_NODE_DB_FOLDER, build_ldk_node, run_cli, run_public,
};
use fedimint_logging::{LOG_GATEWAY, TracingSetup};
#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
use tikv_jemallocator::Jemalloc;
use tracing::info;

#[cfg(not(any(target_env = "msvc", target_os = "ios", target_os = "android")))]
#[global_allocator]
// rocksdb suffers from memory fragmentation when using standard allocator
static GLOBAL: Jemalloc = Jemalloc;

fn main() -> anyhow::Result<()> {
    handle_version_hash_command(fedimint_build_code_version_env!());
    TracingSetup::default().init()?;

    // 1. Parse CLI args and create the runtime.
    let opts = GatewayOpts::parse();
    let runtime = Arc::new(tokio::runtime::Runtime::new()?);

    info!(target: LOG_GATEWAY, version = %fedimint_build_code_version_env!(), "Starting gatewaydv2");

    runtime.block_on(install_crypto_provider());

    // 2. Open the database, ensure the mnemonic, and build the client factory.
    let (gateway_db, client_builder, mnemonic) =
        runtime.block_on(Gateway::prepare(&opts.data_dir))?;

    // 3. Build and start the LDK node. Passing the runtime here is what lets the
    //    node be `Arc<Node>` on `Gateway` rather than lazily created later.
    let node = build_ldk_node(
        &opts.data_dir.join(LDK_NODE_DB_FOLDER),
        &Gateway::chain_source(&opts),
        opts.network,
        opts.ldk_addr,
        opts.ldk_alias.clone(),
        mnemonic,
        runtime.clone(),
    )?;

    // 4. Assemble the shared gateway state.
    let gateway = Gateway::new(
        node,
        client_builder,
        gateway_db,
        opts.api_addr,
        opts.network,
        opts.default_routing_fees,
        opts.default_transaction_fees,
    );

    // 5. Fire-and-forget every long-running task. Federation clients are
    //    lazy-loaded on first use; all work is persisted incrementally and
    //    idempotent on retry, so the runtime drop on process exit aborts cleanly.
    //    Boot reconciliation re-drives incoming payments interrupted by a previous
    //    shutdown.
    runtime.spawn(gateway.clone().process_ldk_events());
    runtime.spawn(run_cli(gateway.clone()));
    runtime.spawn(run_public(gateway.clone()));
    runtime.spawn(gateway.reconcile_pending_claims());

    // 6. Block main on SIGTERM so the runtime stays alive; on signal, return and
    //    let the runtime drop abort all tasks.
    runtime.block_on(shutdown_signal());

    info!(target: LOG_GATEWAY, "Gatewaydv2 exiting...");

    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("Failed to install SIGTERM handler")
        .recv()
        .await;
}
