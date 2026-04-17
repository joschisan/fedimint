use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use picomint_gateway_cli_core::{
    FederationBalanceResponse, InfoResponse, LdkChannelListResponse, LdkInvoiceCreateResponse,
    LdkOnchainReceiveResponse,
};
use picomint_server_cli_core::{InviteResponse, SetupStatus};
use serde::de::DeserializeOwned;
use serde_json::Value;

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

fn gateway_cmd(gw_data_dir: &Path) -> Command {
    let mut cmd = Command::new("target/debug/picomint-gateway-cli");
    cmd.arg("--data-dir").arg(gw_data_dir);
    cmd
}

fn server_cmd(data_dir: &Path) -> Command {
    let mut cmd = Command::new("target/debug/picomint-server-cli");
    cmd.arg("--data-dir").arg(data_dir);
    cmd
}

/// Helper to compute a guardian's data directory from the shared test
/// temp root, mirroring `env::start_picomintd`'s layout.
pub fn guardian_data_dir(base: &Path, peer: usize) -> PathBuf {
    base.join(format!("picomintd-{peer}"))
}

// ── Gateway CLI wrappers ────────────────────────────────────────────────────

pub fn gatewayd_info(gw_data_dir: &Path) -> Result<InfoResponse> {
    gateway_cmd(gw_data_dir)
        .arg("info")
        .run_cli::<InfoResponse>()
}

pub fn gatewayd_federation_join(gw_data_dir: &Path, invite: &str) -> Result<Value> {
    gateway_cmd(gw_data_dir)
        .arg("federation")
        .arg("join")
        .arg(invite)
        .run_cli::<Value>()
}

pub fn gatewayd_federation_balance(
    gw_data_dir: &Path,
    fed_id: &str,
) -> Result<FederationBalanceResponse> {
    gateway_cmd(gw_data_dir)
        .arg("federation")
        .arg("balance")
        .arg(fed_id)
        .run_cli::<FederationBalanceResponse>()
}

pub fn gatewayd_ldk_onchain_receive(gw_data_dir: &Path) -> Result<LdkOnchainReceiveResponse> {
    gateway_cmd(gw_data_dir)
        .arg("ldk")
        .arg("onchain")
        .arg("receive")
        .run_cli::<LdkOnchainReceiveResponse>()
}

pub fn gatewayd_ldk_channel_open(
    gw_data_dir: &Path,
    node_id: &str,
    ln_addr: &str,
    channel_sats: u64,
    push_sats: u64,
) -> Result<Value> {
    gateway_cmd(gw_data_dir)
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

pub fn gatewayd_ldk_channel_list(gw_data_dir: &Path) -> Result<LdkChannelListResponse> {
    gateway_cmd(gw_data_dir)
        .arg("ldk")
        .arg("channel")
        .arg("list")
        .run_cli::<LdkChannelListResponse>()
}

pub fn gatewayd_ldk_invoice_create(
    gw_data_dir: &Path,
    amount_msat: u64,
) -> Result<LdkInvoiceCreateResponse> {
    gateway_cmd(gw_data_dir)
        .arg("ldk")
        .arg("invoice")
        .arg("create")
        .arg(amount_msat.to_string())
        .run_cli::<LdkInvoiceCreateResponse>()
}

pub fn gatewayd_ldk_invoice_pay(gw_data_dir: &Path, invoice: &str) -> Result<Value> {
    gateway_cmd(gw_data_dir)
        .arg("ldk")
        .arg("invoice")
        .arg("pay")
        .arg(invoice)
        .run_cli::<Value>()
}

// ── Guardian CLI wrappers ───────────────────────────────────────────────────

pub fn picomintd_invite(data_dir: &Path) -> Result<InviteResponse> {
    server_cmd(data_dir)
        .arg("invite")
        .run_cli::<InviteResponse>()
}

pub fn picomintd_setup_status(data_dir: &Path) -> Result<SetupStatus> {
    server_cmd(data_dir)
        .arg("setup")
        .arg("status")
        .run_cli::<SetupStatus>()
}

pub fn picomintd_setup_set_local_params(
    data_dir: &Path,
    name: &str,
    federation_name: Option<&str>,
    federation_size: Option<u32>,
) -> Result<Value> {
    let mut cmd = server_cmd(data_dir);
    cmd.arg("setup").arg("set-local-params").arg(name);
    if let Some(fed_name) = federation_name {
        cmd.arg("--federation-name").arg(fed_name);
    }
    if let Some(size) = federation_size {
        cmd.arg("--federation-size").arg(size.to_string());
    }
    cmd.run_cli::<Value>()
}

pub fn picomintd_setup_add_peer(data_dir: &Path, setup_code: &str) -> Result<Value> {
    server_cmd(data_dir)
        .arg("setup")
        .arg("add-peer")
        .arg(setup_code)
        .run_cli::<Value>()
}

pub fn picomintd_setup_start_dkg(data_dir: &Path) -> Result<Value> {
    server_cmd(data_dir)
        .arg("setup")
        .arg("start-dkg")
        .run_cli::<Value>()
}

pub fn picomintd_setup_restore(data_dir: &Path, config_path: &Path) -> Result<Value> {
    server_cmd(data_dir)
        .arg("setup")
        .arg("restore")
        .arg(config_path)
        .run_cli::<Value>()
}

pub fn picomintd_config(data_dir: &Path) -> Result<Value> {
    server_cmd(data_dir).arg("config").run_cli::<Value>()
}

pub fn picomintd_session_count(data_dir: &Path) -> Result<u64> {
    server_cmd(data_dir).arg("session-count").run_cli::<u64>()
}

pub fn picomintd_ln_gateway_add(data_dir: &Path, gateway: &str) -> Result<bool> {
    server_cmd(data_dir)
        .arg("module")
        .arg("ln")
        .arg("gateway")
        .arg("add")
        .arg(gateway)
        .run_cli::<bool>()
}

pub fn picomintd_ln_gateway_remove(data_dir: &Path, gateway: &str) -> Result<bool> {
    server_cmd(data_dir)
        .arg("module")
        .arg("ln")
        .arg("gateway")
        .arg("remove")
        .arg(gateway)
        .run_cli::<bool>()
}
