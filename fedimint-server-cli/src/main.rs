use anyhow::{Context, Result, ensure};
use clap::{Parser, Subcommand};
use fedimint_server_cli_core::{
    Lnv2GatewayRequest, ROUTE_AUDIT, ROUTE_INVITE, ROUTE_MODULE_LNV2_GATEWAY_ADD,
    ROUTE_MODULE_LNV2_GATEWAY_LIST, ROUTE_MODULE_LNV2_GATEWAY_REMOVE,
    ROUTE_MODULE_WALLET_BLOCK_COUNT, ROUTE_MODULE_WALLET_FEERATE,
    ROUTE_MODULE_WALLET_PENDING_TX_CHAIN, ROUTE_MODULE_WALLET_TOTAL_VALUE,
    ROUTE_MODULE_WALLET_TX_CHAIN, ROUTE_SETUP_ADD_PEER, ROUTE_SETUP_SET_LOCAL_PARAMS,
    ROUTE_SETUP_START_DKG, ROUTE_SETUP_STATUS, SetupAddPeerRequest, SetupSetLocalParamsRequest,
};
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
    /// Get federation invite code
    Invite,
    /// Show federation audit summary
    Audit,
    /// Module admin commands
    #[command(subcommand)]
    Module(ModuleCommands),
}

#[derive(Subcommand)]
enum SetupCommands {
    /// Check setup status
    Status,
    /// Set local guardian parameters
    SetLocalParams {
        /// Guardian name
        name: String,
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

#[derive(Subcommand)]
enum ModuleCommands {
    /// Wallet module commands
    #[command(subcommand)]
    Walletv2(WalletCommands),
    /// LNv2 module commands
    #[command(subcommand)]
    Lnv2(Lnv2Commands),
}

#[derive(Subcommand)]
enum WalletCommands {
    /// Get total wallet value
    TotalValue,
    /// Get consensus block count
    BlockCount,
    /// Get consensus fee rate
    Feerate,
    /// Get pending transaction chain
    PendingTxChain,
    /// Get transaction chain
    TxChain,
}

#[derive(Subcommand)]
enum Lnv2Commands {
    /// Gateway management
    #[command(subcommand)]
    Gateway(Lnv2GatewayCommands),
}

#[derive(Subcommand)]
enum Lnv2GatewayCommands {
    /// Add a vetted gateway
    Add { url: String },
    /// Remove a vetted gateway
    Remove { url: String },
    /// List vetted gateways
    List,
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
        Commands::Invite => request(addr, ROUTE_INVITE, ())?,
        Commands::Audit => request(addr, ROUTE_AUDIT, ())?,
        Commands::Setup(cmd) => match cmd {
            SetupCommands::Status => request(addr, ROUTE_SETUP_STATUS, ())?,
            SetupCommands::SetLocalParams {
                name,
                federation_name,
                federation_size,
            } => request(
                addr,
                ROUTE_SETUP_SET_LOCAL_PARAMS,
                SetupSetLocalParamsRequest {
                    name,
                    federation_name,
                    federation_size,
                },
            )?,
            SetupCommands::AddPeer { setup_code } => {
                request(addr, ROUTE_SETUP_ADD_PEER, SetupAddPeerRequest { setup_code })?
            }
            SetupCommands::StartDkg => request(addr, ROUTE_SETUP_START_DKG, ())?,
        },
        Commands::Module(cmd) => match cmd {
            ModuleCommands::Walletv2(cmd) => match cmd {
                WalletCommands::TotalValue => {
                    request(addr, ROUTE_MODULE_WALLET_TOTAL_VALUE, ())?
                }
                WalletCommands::BlockCount => {
                    request(addr, ROUTE_MODULE_WALLET_BLOCK_COUNT, ())?
                }
                WalletCommands::Feerate => request(addr, ROUTE_MODULE_WALLET_FEERATE, ())?,
                WalletCommands::PendingTxChain => {
                    request(addr, ROUTE_MODULE_WALLET_PENDING_TX_CHAIN, ())?
                }
                WalletCommands::TxChain => request(addr, ROUTE_MODULE_WALLET_TX_CHAIN, ())?,
            },
            ModuleCommands::Lnv2(cmd) => match cmd {
                Lnv2Commands::Gateway(cmd) => match cmd {
                    Lnv2GatewayCommands::Add { url } => request(
                        addr,
                        ROUTE_MODULE_LNV2_GATEWAY_ADD,
                        Lnv2GatewayRequest { url },
                    )?,
                    Lnv2GatewayCommands::Remove { url } => request(
                        addr,
                        ROUTE_MODULE_LNV2_GATEWAY_REMOVE,
                        Lnv2GatewayRequest { url },
                    )?,
                    Lnv2GatewayCommands::List => {
                        request(addr, ROUTE_MODULE_LNV2_GATEWAY_LIST, ())?
                    }
                },
            },
        },
    };

    print_json(&result);
    Ok(())
}
