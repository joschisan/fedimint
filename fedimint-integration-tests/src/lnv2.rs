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
    test_direct_ln_payments(env).await?;
    test_payments(env).await?;
    test_gateway_registration(env).await?;

    Ok(())
}

async fn test_direct_ln_payments(env: &TestEnv) -> anyhow::Result<()> {
    info!("lnv2: test_direct_ln_payments");

    info!("Gateway pays LDK node invoice...");
    {
        let invoice = env.ldk_node.bolt11_payment().receive(
            1_000_000,
            &lightning_invoice::Bolt11InvoiceDescription::Direct(
                lightning_invoice::Description::new(String::new())?,
            ),
            3600,
        )?;

        cli::gatewayd_ldk_invoice_pay(&env.gw_addr, &invoice.to_string())?;
    }

    info!("LDK node pays gateway invoice...");
    {
        let invoice_str = cli::gatewayd_ldk_invoice_create(&env.gw_addr, 1_000_000)?.invoice;
        let invoice: lightning_invoice::Bolt11Invoice = invoice_str.parse()?;

        // The freestanding node may need a moment to consider the channel ready
        // for outbound payments after the gateway-initiated handshake.
        crate::env::retry("ldk node pays gateway", || async {
            env.ldk_node
                .bolt11_payment()
                .send(&invoice, None)
                .map(|_| ())
                .map_err(|e| anyhow::anyhow!("send failed: {e:?}"))
        })
        .await?;
    }

    info!("lnv2: test_direct_ln_payments passed");

    Ok(())
}

async fn test_gateway_registration(env: &TestEnv) -> anyhow::Result<()> {
    info!("lnv2: test_gateway_registration");

    let client = env.new_client().await?;
    let lnv2 = client.get_first_module::<LightningClientModule>()?;

    let gateway = env.gw_public.clone();

    info!("Testing registration of gateway...");

    for peer in 0..NUM_GUARDIANS {
        assert!(cli::fedimintd_lnv2_gateway_add(peer, &gateway)?);
    }

    let listed = lnv2.list_gateways(None).await?;
    assert_eq!(listed.len(), 1);

    let listed = lnv2
        .list_gateways(Some(fedimint_core::PeerId::from(0)))
        .await?;
    assert_eq!(listed.len(), 1);

    info!("Testing deregistration of gateway...");

    for peer in 0..NUM_GUARDIANS {
        assert!(cli::fedimintd_lnv2_gateway_remove(peer, &gateway)?);
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

    let gw: SafeUrl = env.gw_public.parse()?;

    info!("Pegging in gateway...");

    env.pegin_gateway(bitcoin::Amount::from_sat(100_000_000))
        .await?;

    info!("Testing payment from client to LDK node...");

    {
        let invoice = env.ldk_node.bolt11_payment().receive(
            1_000_000,
            &lightning_invoice::Bolt11InvoiceDescription::Direct(
                lightning_invoice::Description::new(String::new())?,
            ),
            3600,
        )?;

        let send_op = lnv2.send(invoice, Some(gw.clone()), Value::Null).await?;
        let state = lnv2.await_final_send_operation_state(send_op).await?;
        assert_eq!(state, FinalSendOperationState::Success);
    }

    info!("Testing payment from LDK node to client...");

    {
        let (invoice, receive_op) = lnv2
            .receive(
                Amount::from_msats(1_000_000),
                300,
                Bolt11InvoiceDescription::Direct(String::new()),
                Some(gw.clone()),
                Value::Null,
            )
            .await?;

        env.ldk_node.bolt11_payment().send(&invoice, None)?;

        let state = lnv2.await_final_receive_operation_state(receive_op).await?;
        assert_eq!(state, FinalReceiveOperationState::Claimed);
    }

    info!("Testing refund when the payee fails the payment...");

    {
        let payment_hash = lightning_types::payment::PaymentHash([0; 32]);

        let invoice = env.ldk_node.bolt11_payment().receive_for_hash(
            1_000_000,
            &lightning_invoice::Bolt11InvoiceDescription::Direct(
                lightning_invoice::Description::new(String::new())?,
            ),
            3600,
            payment_hash,
        )?;

        let send_op = lnv2.send(invoice, Some(gw.clone()), Value::Null).await?;

        // Wait until the HTLC is actually held by LDK, then fail it. Failing
        // before the HTLC arrives is a no-op in LDK's ChannelManager, so the
        // HTLC would sit held and the contract would never cancel.
        loop {
            let event = env.ldk_node.next_event_async().await;
            env.ldk_node.event_handled()?;
            if let ldk_node::Event::PaymentClaimable {
                payment_hash: hash, ..
            } = event
            {
                if hash == payment_hash {
                    break;
                }
            }
        }
        env.ldk_node.bolt11_payment().fail_for_hash(payment_hash)?;

        let state = lnv2.await_final_send_operation_state(send_op).await?;
        assert_eq!(state, FinalSendOperationState::Refunded);
    }

    info!("lnv2: test_payments passed");

    Ok(())
}
