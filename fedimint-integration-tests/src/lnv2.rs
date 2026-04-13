use fedimint_core::Amount;
use fedimint_core::util::SafeUrl;
use fedimint_lnv2_client::{
    FinalReceiveOperationState, FinalSendOperationState, LightningClientModule,
};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use serde_json::Value;
use tracing::info;

use crate::cli;
use crate::env::{NUM_GUARDIANS, TestEnv};

pub async fn run_tests(env: &TestEnv) -> anyhow::Result<()> {
    test_gateway_registration(env).await?;
    test_payments(env).await?;

    Ok(())
}

async fn test_gateway_registration(env: &TestEnv) -> anyhow::Result<()> {
    info!("lnv2: test_gateway_registration");

    let client = env.new_client().await?;
    let lnv2 = client.get_first_module::<LightningClientModule>()?;

    let gateways = [env.gw1_public.clone(), env.gw2_public.clone()];

    info!("Testing registration of gateways...");

    for gateway in &gateways {
        for peer in 0..NUM_GUARDIANS {
            assert!(cli::fedimintd_lnv2_gateway_add(peer, gateway)?);
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
            assert!(cli::fedimintd_lnv2_gateway_remove(peer, gateway)?);
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

    env.pegin(&client, bitcoin::Amount::from_sat(100_000_000))
        .await?;

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

    env.pegin_gateway(&env.gw1_addr, bitcoin::Amount::from_sat(100_000_000))
        .await?;
    env.pegin_gateway(&env.gw2_addr, bitcoin::Amount::from_sat(100_000_000))
        .await?;

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
        let invoice_str = cli::gatewayd_ldk_invoice_create(&env.gw2_addr, 1_000_000)?.invoice;

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

        cli::gatewayd_ldk_invoice_pay(&env.gw2_addr, &invoice.to_string())?;

        let state = lnv2.await_final_receive_operation_state(receive_op).await?;
        assert_eq!(state, FinalReceiveOperationState::Claimed);
    }

    info!("lnv2: test_payments passed");

    Ok(())
}
