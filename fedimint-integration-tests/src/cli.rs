use std::process::Command;

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;

use crate::env::find_binary;

pub trait RunGatewayCli {
    fn run_gateway_cli<T: DeserializeOwned>(&mut self) -> Result<T>;
}

impl RunGatewayCli for Command {
    fn run_gateway_cli<T: DeserializeOwned>(&mut self) -> Result<T> {
        let output = self.output().context("Failed to run gateway-cli")?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            bail!("gateway-cli failed:\nstdout: {stdout}\nstderr: {stderr}",);
        }

        let stdout = String::from_utf8(output.stdout)?;
        serde_json::from_str(stdout.trim())
            .context(format!("Failed to parse gateway-cli output: {stdout}"))
    }
}

pub fn gateway_cmd(gw_addr: &str) -> Command {
    let mut cmd = Command::new(find_binary("gatewayd-cli"));
    cmd.arg("-a").arg(gw_addr);
    cmd
}
