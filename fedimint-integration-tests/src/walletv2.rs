use anyhow::{Context, ensure};
use bitcoincore_rpc::RpcApi;
use fedimint_core::Amount;
use fedimint_walletv2_client::{FinalSendOperationState, WalletClientModule};
use tokio::task::block_in_place;
use tracing::info;

use crate::env::{TestEnv, retry};

pub async fn run_tests(env: &TestEnv) -> anyhow::Result<()> {
    info!("walletv2: circular_deposit");

    let client_send = env.new_client().await?;
    let client_receive = env.new_client().await?;

    env.pegin(&client_send, bitcoin::Amount::from_sat(100_000_000))
        .await?;

    let receive_address = client_receive
        .get_first_module::<WalletClientModule>()?
        .receive()
        .await;

    info!(
        address = %receive_address,
        "Sending to receiver's federation address"
    );

    let operation_id = client_send
        .get_first_module::<WalletClientModule>()?
        .send(
            receive_address.as_unchecked().clone(),
            bitcoin::Amount::from_sat(100_000),
            None,
        )
        .await?;

    let state = client_send
        .get_first_module::<WalletClientModule>()?
        .await_final_send_operation_state(operation_id)
        .await;

    let FinalSendOperationState::Success(txid) = state else {
        panic!("Circular deposit send operation failed: {state:?}");
    };

    info!(%txid, "Send confirmed, waiting for tx in mempool");

    retry("send tx in mempool", || async {
        block_in_place(|| env.bitcoind.get_mempool_entry(&txid))
            .map(|_| ())
            .context("send tx not in mempool yet")
    })
    .await?;

    env.mine_blocks(10);

    retry("circular deposit balance", || async {
        let balance = client_receive.get_balance().await?;

        ensure!(
            balance >= Amount::from_sats(99_000),
            "receiver balance {balance} too low"
        );

        Ok(())
    })
    .await?;

    info!("walletv2: circular_deposit passed");

    Ok(())
}
