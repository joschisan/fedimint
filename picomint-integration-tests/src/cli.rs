use std::process::Command;

use anyhow::{Context, Result, bail};
use picomint_gateway_cli_core::{
    FederationBalanceResponse, InfoResponse, LdkChannelListResponse, LdkInvoiceCreateResponse,
    LdkOnchainReceiveResponse,
};
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::env::{GUARDIAN_BASE_PORT, PORTS_PER_GUARDIAN};

trait RunCli {
    fn run_cli<T: DeserializeOwned>(&mut self) -> Result<T>;
}

impl RunCli for Command {
    fn run_cli<T: DeserializeOwned>(&mut self) -> Result<T> {
        let output = self.output().context("Failed to run CLI")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!("CLI failed:\nstdout: {stdout}\nstderr: {stderr}");
        }

        let stdout = String::from_utf8(output.stdout)?;
        serde_json::from_str(stdout.trim()).context(format!("Failed to parse CLI output: {stdout}"))
    }
}

pub fn gatewayd_info(gw_addr: &str) -> Result<InfoResponse> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("info")
        .run_cli::<InfoResponse>()
}

pub fn gatewayd_federation_join(gw_addr: &str, invite: &str) -> Result<Value> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("federation")
        .arg("join")
        .arg(invite)
        .run_cli::<Value>()
}

pub fn gatewayd_federation_balance(
    gw_addr: &str,
    fed_id: &str,
) -> Result<FederationBalanceResponse> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("federation")
        .arg("balance")
        .arg(fed_id)
        .run_cli::<FederationBalanceResponse>()
}

pub fn gatewayd_ldk_onchain_receive(gw_addr: &str) -> Result<LdkOnchainReceiveResponse> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("ldk")
        .arg("onchain")
        .arg("receive")
        .run_cli::<LdkOnchainReceiveResponse>()
}

pub fn gatewayd_ldk_channel_open(
    gw_addr: &str,
    node_id: &str,
    ln_addr: &str,
    channel_sats: u64,
    push_sats: u64,
) -> Result<Value> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("ldk")
        .arg("channel")
        .arg("open")
        .arg(node_id)
        .arg(ln_addr)
        .arg(channel_sats.to_string())
        .arg("--push-amount-sats")
        .arg(push_sats.to_string())
        .run_cli::<Value>()
}

pub fn gatewayd_ldk_channel_list(gw_addr: &str) -> Result<LdkChannelListResponse> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("ldk")
        .arg("channel")
        .arg("list")
        .run_cli::<LdkChannelListResponse>()
}

pub fn gatewayd_ldk_invoice_create(
    gw_addr: &str,
    amount_msat: u64,
) -> Result<LdkInvoiceCreateResponse> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("ldk")
        .arg("invoice")
        .arg("create")
        .arg(amount_msat.to_string())
        .run_cli::<LdkInvoiceCreateResponse>()
}

pub fn gatewayd_ldk_invoice_pay(gw_addr: &str, invoice: &str) -> Result<Value> {
    Command::new("target/debug/picomint-gateway-cli")
        .arg("-a")
        .arg(gw_addr)
        .arg("ldk")
        .arg("invoice")
        .arg("pay")
        .arg(invoice)
        .run_cli::<Value>()
}

pub fn picomintd_ln_gateway_add(peer: usize, gateway: &str) -> Result<bool> {
    let cli_port = GUARDIAN_BASE_PORT + (peer as u16 * PORTS_PER_GUARDIAN) + 4;

    Command::new("target/debug/picomint-server-cli")
        .arg("-a")
        .arg(format!("http://127.0.0.1:{cli_port}"))
        .arg("module")
        .arg("ln")
        .arg("gateway")
        .arg("add")
        .arg(gateway)
        .run_cli::<bool>()
}

pub fn picomintd_ln_gateway_remove(peer: usize, gateway: &str) -> Result<bool> {
    let cli_port = GUARDIAN_BASE_PORT + (peer as u16 * PORTS_PER_GUARDIAN) + 4;

    Command::new("target/debug/picomint-server-cli")
        .arg("-a")
        .arg(format!("http://127.0.0.1:{cli_port}"))
        .arg("module")
        .arg("ln")
        .arg("gateway")
        .arg("remove")
        .arg(gateway)
        .run_cli::<bool>()
}
