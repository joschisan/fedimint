mod env;
mod lnv2;
mod mintv2;
mod walletv2;

use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // SAFETY: Called before any threads are spawned
    unsafe { std::env::set_var("FM_IN_DEVIMINT", "1") };

    fedimint_logging::TracingSetup::default().init()?;

    info!("Setting up test environment...");
    let env = env::TestEnv::setup().await?;

    info!("Test environment ready!");
    info!("Invite code: {}", env.invite_code);
    info!("Gateway 1: {}", env.gw1_addr);
    info!("Gateway 2: {}", env.gw2_addr);

    info!("Running lnv2 tests...");
    lnv2::run_tests(&env).await?;

    info!("Running mintv2 tests...");
    mintv2::run_tests(&env).await?;

    info!("Running walletv2 tests...");
    walletv2::run_tests(&env).await?;

    info!("All integration tests passed!");
    Ok(())
}
