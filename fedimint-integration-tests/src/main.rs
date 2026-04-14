mod cli;
mod env;
mod lnv2;
mod mintv2;
mod walletv2;

use std::sync::Arc;

use tracing::info;

fn main() -> anyhow::Result<()> {
    // SAFETY: Called before any threads are spawned
    unsafe { std::env::set_var("FM_IN_DEVIMINT", "1") };

    fedimint_logging::TracingSetup::default().init()?;

    let runtime = Arc::new(tokio::runtime::Runtime::new()?);

    info!("Setting up test environment...");
    let env = env::TestEnv::setup(runtime.clone())?;

    info!("Test environment ready!");
    info!("Invite code: {}", env.invite_code);
    info!("Gateway: {}", env.gw_addr);

    let client_send = runtime.block_on(env.new_client())?;

    info!("Running walletv2 tests...");
    runtime.block_on(walletv2::run_tests(&env, &client_send))?;

    info!("Running lnv2 tests...");
    runtime.block_on(lnv2::run_tests(&env, &client_send))?;

    info!("Running mintv2 tests...");
    runtime.block_on(mintv2::run_tests(&env, &client_send))?;

    info!("All integration tests passed!");

    runtime.block_on(env.shutdown());
    Ok(())
}
