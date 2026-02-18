use clap::Parser;
use devimint::cmd;
use devimint::devfed::DevJitFed;
use fedimint_core::envs::FM_ENABLE_MODULE_MINTV2_ENV;
use tracing::info;

#[derive(Parser)]
#[command(name = "mintv2-module-tests")]
#[command(about = "MintV2 module integration tests", long_about = None)]
struct Cli {}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _cli = Cli::parse();

    // Enable MintV2 module for these tests
    // TODO: Audit that the environment access only happens in single-threaded code.
    unsafe { std::env::set_var(FM_ENABLE_MODULE_MINTV2_ENV, "1") };

    devimint::run_devfed_test()
        .call(|dev_fed, _process_mgr| async move { test_mintv2(&dev_fed).await })
        .await
}

async fn test_mintv2(dev_fed: &DevJitFed) -> anyhow::Result<()> {
    let federation = dev_fed.fed().await?;

    let client = federation.new_joined_client("mintv2-test-client").await?;

    info!("Testing peg-in...");

    federation.pegin_client(10_000, &client).await?;

    info!("Testing ecash send...");

    let ecash = cmd!(client, "module", "mintv2", "send", "1000000")
        .out_json()
        .await?
        .as_str()
        .expect("ecash should be a string")
        .to_string();

    info!("Testing ecash receive...");

    cmd!(client, "module", "mintv2", "receive", ecash)
        .out_json()
        .await?;

    info!("MintV2 module tests complete!");

    Ok(())
}
