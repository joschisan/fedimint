//! Thin wrappers around the `gatewaydv2-cli` admin CLI, which drives the
//! `gatewaydv2` (LDK + LNv2 only) daemon over its Unix socket.
//!
//! Mirrors picomint's `picomint-integration-tests/src/cli.rs`: each function
//! shells out to `gatewaydv2-cli --data-dir <dir> <subcommand...>` and parses
//! the JSON response into the shared `fedimint-gatewayv2-cli-core` types. Kept
//! in its own module so the v1 `GatewayClient` in `gatewayd.rs` only needs a
//! one-line delegate per v2 branch.

use std::path::Path;
use std::str::FromStr;

use anyhow::{Context, Result};
use esplora_client::Txid;
use fedimint_gateway_common::ChannelInfo;
use fedimint_gatewayv2_cli_core::{
    FederationBalanceResponse, FederationWalletReceiveResponse, InfoResponse,
    LdkChannelListResponse, LdkLnReceiveResponse, LdkOnchainReceiveResponse,
};
use fedimint_ln_server::common::lightning_invoice::Bolt11Invoice;
use serde_json::Value;

use crate::cmd;
use crate::util::{JsonValueExt, get_gatewayv2_cli_path, poll_simple};
use crate::vars::utf8;

/// Base argv for the admin CLI: `gatewaydv2-cli --data-dir <dir>`. Mirrors
/// picomint's `gateway_cmd`; each wrapper appends its subcommand.
fn gateway_cmd(data_dir: &Path) -> Vec<String> {
    let mut argv = get_gatewayv2_cli_path();
    argv.push("--data-dir".to_string());
    argv.push(utf8(data_dir).to_string());
    argv
}

/// `gatewaydv2-cli info`
pub async fn info(data_dir: &Path) -> Result<InfoResponse> {
    cmd!(gateway_cmd(data_dir), "info")
        .out_json()
        .await?
        .to_typed()
}

/// `gatewaydv2-cli federation join <invite>`
pub async fn federation_join(data_dir: &Path, invite: &str) -> Result<Value> {
    cmd!(gateway_cmd(data_dir), "federation", "join", invite)
        .out_json()
        .await
}

/// `gatewaydv2-cli federation balance <fed_id>` -> ecash balance in msats.
pub async fn federation_balance(data_dir: &Path, fed_id: &str) -> Result<u64> {
    let resp: FederationBalanceResponse =
        cmd!(gateway_cmd(data_dir), "federation", "balance", fed_id)
            .out_json()
            .await?
            .to_typed()?;
    Ok(resp.balance_msat.msats)
}

/// `gatewaydv2-cli federation module wallet receive <fed_id>` -> deposit
/// address as a string.
pub async fn wallet_receive_address(data_dir: &Path, fed_id: &str) -> Result<String> {
    let resp: FederationWalletReceiveResponse = cmd!(
        gateway_cmd(data_dir),
        "federation",
        "module",
        "wallet",
        "receive",
        fed_id
    )
    .out_json()
    .await?
    .to_typed()?;
    Ok(resp.address.assume_checked().to_string())
}

/// `gatewaydv2-cli ldk onchain receive` -> funding address as a string.
pub async fn onchain_receive_address(data_dir: &Path) -> Result<String> {
    let resp: LdkOnchainReceiveResponse = cmd!(gateway_cmd(data_dir), "ldk", "onchain", "receive")
        .out_json()
        .await?
        .to_typed()?;
    Ok(resp.address.assume_checked().to_string())
}

/// `gatewaydv2-cli ldk channel open <pubkey> <host> <size_sat> ...` -> funding
/// transaction txid.
pub async fn channel_open(
    data_dir: &Path,
    pubkey: bitcoin::secp256k1::PublicKey,
    host: &str,
    channel_size_sats: u64,
    push_amount_sats: u64,
) -> Result<Txid> {
    cmd!(
        gateway_cmd(data_dir),
        "ldk",
        "channel",
        "open",
        pubkey,
        host,
        channel_size_sats,
        "--push-amount-sat",
        push_amount_sats
    )
    .run()
    .await?;

    // The daemon initiates the open fire-and-forget (picomint-style): the
    // funding transaction is negotiated and broadcast asynchronously. Poll the
    // channel list until LDK reports the funding txid so callers can wait for
    // it in the mempool before mining confirmation blocks.
    poll_simple("gatewaydv2 channel funding txid", || async {
        let resp: LdkChannelListResponse = cmd!(gateway_cmd(data_dir), "ldk", "channel", "list")
            .out_json()
            .await?
            .to_typed()?;

        let funding_txid = resp
            .channels
            .iter()
            .find(|channel| channel.remote_pubkey.to_string() == pubkey.to_string())
            .and_then(|channel| channel.funding_txid)
            .context("channel does not have a funding txid yet")?;

        Ok(Txid::from_str(&funding_txid.to_string())?)
    })
    .await
}

/// `gatewaydv2-cli ldk channel list`, mapped to devimint's [`ChannelInfo`].
pub async fn channel_list(data_dir: &Path) -> Result<Vec<ChannelInfo>> {
    let resp: LdkChannelListResponse = cmd!(gateway_cmd(data_dir), "ldk", "channel", "list")
        .out_json()
        .await?
        .to_typed()?;
    Ok(resp
        .channels
        .into_iter()
        .map(|c| ChannelInfo {
            remote_pubkey: c.remote_pubkey,
            channel_size_sats: c.channel_size_sat,
            outbound_liquidity_sats: c.outbound_liquidity_sat,
            inbound_liquidity_sats: c.inbound_liquidity_sat,
            is_active: c.is_usable,
            // The admin CLI only exposes the funding txid; devimint only reads
            // `funding_outpoint` for the v1 set-channel-fees path, which v2 does
            // not use.
            funding_outpoint: None,
            remote_node_alias: c.remote_alias,
            remote_address: c.remote_address,
            base_fee_msat: None,
            parts_per_million: None,
        })
        .collect())
}

/// `gatewaydv2-cli ldk ln receive <amount_msat>` -> bolt11 invoice.
pub async fn ln_receive(data_dir: &Path, amount_msats: u64) -> Result<Bolt11Invoice> {
    let resp: LdkLnReceiveResponse =
        cmd!(gateway_cmd(data_dir), "ldk", "ln", "receive", amount_msats)
            .out_json()
            .await?
            .to_typed()?;
    Ok(Bolt11Invoice::from_str(&resp.invoice)?)
}

/// `gatewaydv2-cli ldk ln send <invoice>`
pub async fn ln_send(data_dir: &Path, invoice: &Bolt11Invoice) -> Result<()> {
    cmd!(
        gateway_cmd(data_dir),
        "ldk",
        "ln",
        "send",
        invoice.to_string()
    )
    .run()
    .await
}
