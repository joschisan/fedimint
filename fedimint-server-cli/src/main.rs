use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fedimint_server_cli_core::*;
use serde::Serialize;
use serde_json::Value;

#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Server admin API address
    #[arg(
        short,
        long,
        env = "FM_SERVER_ADDR",
        default_value = "http://127.0.0.1:8177"
    )]
    address: String,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Setup commands (DKG)
    #[command(subcommand)]
    Setup(SetupCommands),
}

#[derive(Subcommand)]
enum SetupCommands {
    /// Check setup status
    Status,
    /// Set local guardian parameters
    SetLocalParams {
        /// Guardian name
        name: String,
        /// Guardian password
        #[arg(long)]
        password: String,
        /// Federation name (leader only)
        #[arg(long)]
        federation_name: Option<String>,
        /// Federation size (leader only)
        #[arg(long)]
        federation_size: Option<u32>,
    },
    /// Add a peer's setup code
    AddPeer {
        /// Peer's setup code
        setup_code: String,
    },
    /// Start distributed key generation
    StartDkg,
}

fn request<R: Serialize>(addr: &str, route: &str, payload: R) -> Result<Value> {
    let response = reqwest::blocking::Client::new()
        .post(format!("{addr}{route}"))
        .json(&serde_json::to_value(payload)?)
        .send()
        .context("Failed to connect to server")?;

    ensure!(
        response.status().is_success(),
        "API error ({}): {}",
        response.status().as_u16(),
        response.text()?
    );

    let text = response.text()?;
    if text.trim().is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_str(&text).context("Failed to parse response")
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("Cannot serialize")
    );
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let addr = &cli.address;

    let result = match cli.command {
        Commands::Setup(cmd) => match cmd {
            SetupCommands::Status => request(addr, ROUTE_SETUP_STATUS, ())?,
            SetupCommands::SetLocalParams {
                name,
                password,
                federation_name,
                federation_size,
            } => request(
                addr,
                ROUTE_SETUP_SET_LOCAL_PARAMS,
                SetLocalParamsRequest {
                    password,
                    name,
                    federation_name,
                    federation_size,
                },
            )?,
            SetupCommands::AddPeer { setup_code } => {
                request(addr, ROUTE_SETUP_ADD_PEER, AddPeerRequest { setup_code })?
            }
            SetupCommands::StartDkg => request(addr, ROUTE_SETUP_START_DKG, ())?,
        },
    };

    print_json(&result);
    Ok(())
}
