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

    runtime.clone().block_on(async move {
        info!("Setting up test environment...");
        let env = env::TestEnv::setup(runtime).await?;

        info!("Test environment ready!");
        info!("Invite code: {}", env.invite_code);
        info!("Gateway: {}", env.gw_addr);

        info!("Running lnv2 tests...");
        lnv2::run_tests(&env).await?;

        info!("Running mintv2 tests...");
        mintv2::run_tests(&env).await?;

        info!("Running walletv2 tests...");
        walletv2::run_tests(&env).await?;

        info!("All integration tests passed!");
        Ok(())
    })
}
