use std::path::Path;

use fedimint_core::Amount;
use fedimint_core::util::SafeUrl;
use fedimint_lnv2_client::{
    FinalReceiveOperationState, FinalSendOperationState, LightningClientModule,
};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use serde_json::Value;
use tracing::info;

use crate::env::{NUM_GUARDIANS, PASSWORD, TestEnv, fedimint_cli_raw, gateway_cli};

async fn add_gateway(cli_client_dir: &Path, peer: usize, gateway: &str) -> anyhow::Result<bool> {
    let result = fedimint_cli_raw(&[
        "--data-dir",
        cli_client_dir.to_str().expect("valid path"),
        "--our-id",
        &peer.to_string(),
        "--password",
        PASSWORD,
        "module",
        "lnv2",
        "gateways",
        "add",
        gateway,
    ])
    .await?;

    serde_json::from_str(result.trim()).map_err(Into::into)
}

async fn remove_gateway(cli_client_dir: &Path, peer: usize, gateway: &str) -> anyhow::Result<bool> {
    let result = fedimint_cli_raw(&[
        "--data-dir",
        cli_client_dir.to_str().expect("valid path"),
        "--our-id",
        &peer.to_string(),
        "--password",
        PASSWORD,
        "module",
        "lnv2",
        "gateways",
        "remove",
        gateway,
    ])
    .await?;

    serde_json::from_str(result.trim()).map_err(Into::into)
}

pub async fn run_tests(env: &TestEnv) -> anyhow::Result<()> {
    test_gateway_registration(env).await?;
    test_payments(env).await?;

    Ok(())
}

async fn test_gateway_registration(env: &TestEnv) -> anyhow::Result<()> {
    info!("lnv2: test_gateway_registration");

    let client = env.new_client().await?;
    let lnv2 = client.get_first_module::<LightningClientModule>()?;
    let cli_client_dir = env.new_cli_client_dir().await?;

    let gateways = [env.gw1_public.clone(), env.gw2_public.clone()];

    info!("Testing registration of gateways...");

    for gateway in &gateways {
        for peer in 0..NUM_GUARDIANS {
            assert!(add_gateway(&cli_client_dir, peer, gateway).await?);
        }
    }

    let listed = lnv2.list_gateways(None).await?;
    assert_eq!(listed.len(), 2);

    let listed = lnv2
        .list_gateways(Some(fedimint_core::PeerId::from(0)))
        .await?;
    assert_eq!(listed.len(), 2);

    info!("Testing deregistration of gateways...");

    for gateway in &gateways {
        for peer in 0..NUM_GUARDIANS {
            assert!(remove_gateway(&cli_client_dir, peer, gateway).await?);
        }
    }

    let listed = lnv2.list_gateways(None).await?;
    assert!(listed.is_empty());

    let listed = lnv2
        .list_gateways(Some(fedimint_core::PeerId::from(0)))
        .await?;
    assert!(listed.is_empty());

    info!("lnv2: test_gateway_registration passed");

    Ok(())
}

async fn test_payments(env: &TestEnv) -> anyhow::Result<()> {
    info!("lnv2: test_payments");

    let client = env.new_client().await?;

    env.pegin(&client).await?;

    let lnv2 = client.get_first_module::<LightningClientModule>()?;

    let gw1: SafeUrl = env.gw1_public.parse()?;
    let gw2: SafeUrl = env.gw2_public.parse()?;

    // Since both gateways are LDK, same-gateway and cross-gateway are the only
    // unique combinations.
    let gateway_pairs = [(&gw1, &gw2), (&gw2, &gw1)];

    info!("Testing refund of circular payments...");

    for (gw_send, gw_receive) in &gateway_pairs {
        info!(
            gw_send = %gw_send,
            gw_receive = %gw_receive,
            "Testing refund: client -> gw_send -> gw_receive -> client"
        );

        let (invoice, _receive_op) = lnv2
            .receive(
                Amount::from_msats(1_000_000),
                300,
                Bolt11InvoiceDescription::Direct(String::new()),
                Some((*gw_receive).clone()),
                Value::Null,
            )
            .await?;

        let send_op = lnv2
            .send(invoice, Some((*gw_send).clone()), Value::Null)
            .await?;

        let state = lnv2.await_final_send_operation_state(send_op).await?;

        assert_eq!(state, FinalSendOperationState::Refunded);
    }

    info!("Pegging in gateways...");

    env.pegin_gateway(&env.gw1_addr).await?;
    env.pegin_gateway(&env.gw2_addr).await?;

    info!("Testing circular payments...");

    for (gw_send, gw_receive) in &gateway_pairs {
        info!(
            gw_send = %gw_send,
            gw_receive = %gw_receive,
            "Testing payment: client -> gw_send -> gw_receive -> client"
        );

        let (invoice, receive_op) = lnv2
            .receive(
                Amount::from_msats(1_000_000),
                300,
                Bolt11InvoiceDescription::Direct(String::new()),
                Some((*gw_receive).clone()),
                Value::Null,
            )
            .await?;

        let send_op = lnv2
            .send(invoice, Some((*gw_send).clone()), Value::Null)
            .await?;

        let send_state = lnv2.await_final_send_operation_state(send_op).await?;
        assert_eq!(send_state, FinalSendOperationState::Success);

        let receive_state = lnv2.await_final_receive_operation_state(receive_op).await?;
        assert_eq!(receive_state, FinalReceiveOperationState::Claimed);
    }

    info!("Testing payment from client to gateway...");

    {
        let invoice_json =
            gateway_cli(&env.gw2_addr, &["lightning", "create-invoice", "1000000"]).await?;

        let invoice_str = invoice_json["invoice"]
            .as_str()
            .expect("invoice must be a string");

        let invoice: lightning_invoice::Bolt11Invoice = invoice_str.parse()?;

        let send_op = lnv2.send(invoice, Some(gw1.clone()), Value::Null).await?;

        let state = lnv2.await_final_send_operation_state(send_op).await?;
        assert_eq!(state, FinalSendOperationState::Success);
    }

    info!("Testing payment from gateway to client...");

    {
        let (invoice, receive_op) = lnv2
            .receive(
                Amount::from_msats(1_000_000),
                300,
                Bolt11InvoiceDescription::Direct(String::new()),
                Some(gw1.clone()),
                Value::Null,
            )
            .await?;

        gateway_cli(
            &env.gw2_addr,
            &["lightning", "pay-invoice", &invoice.to_string()],
        )
        .await?;

        let state = lnv2.await_final_receive_operation_state(receive_op).await?;
        assert_eq!(state, FinalReceiveOperationState::Claimed);
    }

    info!("lnv2: test_payments passed");

    Ok(())
}
