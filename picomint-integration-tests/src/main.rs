mod cli;
mod env;
mod ln;
mod mint;
mod wallet;

use std::sync::Arc;

use tracing::info;

fn main() -> anyhow::Result<()> {
    // SAFETY: Called before any threads are spawned
    unsafe { std::env::set_var("IN_TEST_ENV", "1") };

    picomint_logging::TracingSetup::default().init()?;

    let runtime = Arc::new(tokio::runtime::Runtime::new()?);

    info!("Setting up test environment...");
    let (env, client_send) = env::TestEnv::setup(runtime.clone())?;

    info!("Test environment ready!");
    info!("Invite code: {}", env.invite_code);
    info!("Gateway: {}", env.gw_addr);

    info!("Running wallet tests...");
    runtime.block_on(wallet::run_tests(&env, &client_send))?;

    info!("Running ln + mint tests in parallel...");
    runtime.block_on(async {
        tokio::try_join!(
            ln::run_tests(&env, &client_send),
            mint::run_tests(&env, &client_send),
        )
    })?;

    info!("Shutting down the primary test client!");

    runtime.block_on(client_send.task_group().clone().shutdown_join_all(None))?;

    info!("All integration tests passed!");

    std::process::exit(0);
}
