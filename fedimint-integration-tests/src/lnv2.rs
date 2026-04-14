use std::pin::pin;

use anyhow::ensure;
use async_stream::stream;
use fedimint_client::ClientHandleArc;
use fedimint_core::Amount;
use fedimint_core::util::SafeUrl;
use fedimint_eventlog::{Event, EventLogEntry, EventLogId};
use fedimint_lnv2_client::LightningClientModule;
use fedimint_lnv2_client::events::{
    ReceivePaymentEvent, SendPaymentEvent, SendPaymentStatus, SendPaymentUpdateEvent,
};
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use futures::StreamExt;
use tracing::info;

use crate::cli;
use crate::env::{NUM_GUARDIANS, TestEnv, retry};

#[derive(Debug)]
enum LnEvent {
    Send(SendPaymentEvent),
    SendUpdate(SendPaymentUpdateEvent),
    Receive(ReceivePaymentEvent),
}

fn ln_event_stream(client: &ClientHandleArc) -> impl futures::Stream<Item = LnEvent> {
    let client = client.clone();
    let mut log_rx = client.log_event_added_rx();
    let mut next_id = EventLogId::LOG_START;

    stream! {
        loop {
            let events = client.get_event_log(Some(next_id), 100).await;

            for entry in events {
                next_id = entry.id().saturating_add(1);

                if let Some(event) = try_parse_ln_event(entry.as_raw()) {
                    yield event;
                }
            }

            let _ = log_rx.changed().await;
        }
    }
}

fn try_parse_ln_event(entry: &EventLogEntry) -> Option<LnEvent> {
    if entry.module_kind() != Some(&fedimint_lnv2_common::KIND) {
        return None;
    }

    if entry.kind == SendPaymentEvent::KIND {
        return entry.to_event().map(LnEvent::Send);
    }

    if entry.kind == SendPaymentUpdateEvent::KIND {
        return entry.to_event().map(LnEvent::SendUpdate);
    }

    if entry.kind == ReceivePaymentEvent::KIND {
        return entry.to_event().map(LnEvent::Receive);
    }

    None
}

pub async fn run_tests(env: &TestEnv, client_send: &ClientHandleArc) -> anyhow::Result<()> {
    test_payments(env, client_send).await?;
    test_gateway_registration(env).await?;
    test_direct_ln_payments(env).await?;

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

    client.task_group().clone().shutdown_join_all(None).await?;

    info!("lnv2: test_gateway_registration passed");

    Ok(())
}

async fn test_payments(env: &TestEnv, client: &ClientHandleArc) -> anyhow::Result<()> {
    info!("lnv2: test_payments");

    let lnv2 = client.get_first_module::<LightningClientModule>()?;

    let gw: SafeUrl = env.gw_public.parse()?;

    let mut events = pin!(ln_event_stream(client));

    info!("Testing payment from client to LDK node (funds gateway federation liquidity)...");

    {
        let invoice = env.ldk_node.bolt11_payment().receive(
            1_000_000,
            &lightning_invoice::Bolt11InvoiceDescription::Direct(
                lightning_invoice::Description::new(String::new())?,
            ),
            3600,
        )?;

        let send_op = lnv2.send(invoice, Some(gw.clone())).await?;

        let Some(LnEvent::Send(send)) = events.next().await else {
            panic!("Expected Send event");
        };
        assert_eq!(send.operation_id, send_op);

        let Some(LnEvent::SendUpdate(update)) = events.next().await else {
            panic!("Expected SendUpdate event");
        };
        assert_eq!(update.operation_id, send_op);
        assert!(matches!(update.status, SendPaymentStatus::Success(_)));
    }

    info!("Polling gateway federation balance...");

    let fed_id = env.invite_code.federation_id().to_string();
    retry("gateway federation balance", || {
        let fed_id = fed_id.clone();
        async move {
            let balance = cli::gatewayd_federation_balance(&env.gw_addr, &fed_id)?.balance_msat;
            ensure!(balance.msats > 0, "gateway federation balance is zero");
            Ok(())
        }
    })
    .await?;

    info!("Testing payment from LDK node to client (half of first send)...");

    {
        let (invoice, receive_op) = lnv2
            .receive(
                Amount::from_msats(500_000),
                300,
                Bolt11InvoiceDescription::Direct(String::new()),
                Some(gw.clone()),
            )
            .await?;

        env.ldk_node.bolt11_payment().send(&invoice, None)?;

        let Some(LnEvent::Receive(receive)) = events.next().await else {
            panic!("Expected Receive event");
        };
        assert_eq!(receive.operation_id, receive_op);

        // Verify the freestanding LDK node observes the payment as successful,
        // i.e. the gateway settled the HTLC back to it via the CompleteSM.
        let payment_hash = lightning_types::payment::PaymentHash(*invoice.payment_hash().as_ref());
        loop {
            let event = env.ldk_node.next_event_async().await;
            env.ldk_node.event_handled()?;
            if let ldk_node::Event::PaymentSuccessful {
                payment_hash: hash, ..
            } = event
                && hash == payment_hash
            {
                break;
            }
        }
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

        let send_op = lnv2.send(invoice, Some(gw.clone())).await?;

        let Some(LnEvent::Send(send)) = events.next().await else {
            panic!("Expected Send event");
        };
        assert_eq!(send.operation_id, send_op);

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

        let Some(LnEvent::SendUpdate(update)) = events.next().await else {
            panic!("Expected SendUpdate event");
        };
        assert_eq!(update.operation_id, send_op);
        assert_eq!(update.status, SendPaymentStatus::Refunded);
    }

    info!("lnv2: test_payments passed");

    Ok(())
}
