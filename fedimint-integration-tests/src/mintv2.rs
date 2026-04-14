use fedimint_core::Amount;
use fedimint_mintv2_client::{FinalReceiveOperationState, MintClientModule};
use tracing::info;

use crate::env::TestEnv;

pub async fn run_tests(env: &TestEnv) -> anyhow::Result<()> {
    info!("mintv2: double_spend_is_rejected");

    let client_send = env.new_client().await?;
    let client_receive = env.new_client().await?;

    env.pegin(&client_send, bitcoin::Amount::from_sat(100_000_000))
        .await?;

    let ecash = client_send
        .get_first_module::<MintClientModule>()?
        .send(Amount::from_sats(1_000))
        .await?;

    // First receive succeeds (sender receives own ecash back)
    let operation_id = client_send
        .get_first_module::<MintClientModule>()?
        .receive(ecash.clone())
        .await?;

    let state = client_send
        .get_first_module::<MintClientModule>()?
        .await_final_receive_operation_state(operation_id)
        .await;

    assert_eq!(state, FinalReceiveOperationState::Success);

    // Second receive with same ecash is rejected
    let operation_id = client_receive
        .get_first_module::<MintClientModule>()?
        .receive(ecash)
        .await?;

    let state = client_receive
        .get_first_module::<MintClientModule>()?
        .await_final_receive_operation_state(operation_id)
        .await;

    assert_eq!(state, FinalReceiveOperationState::Rejected);

    info!("mintv2: double_spend_is_rejected passed");

    Ok(())
}
