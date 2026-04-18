use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, ensure};
use clap::{Parser, Subcommand};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use picomint_server_cli_core::{
    CLI_SOCKET_FILENAME, LnGatewayRequest, ROUTE_AUDIT, ROUTE_CONFIG, ROUTE_INVITE,
    ROUTE_MODULE_LN_GATEWAY_ADD, ROUTE_MODULE_LN_GATEWAY_LIST, ROUTE_MODULE_LN_GATEWAY_REMOVE,
    ROUTE_MODULE_WALLET_BLOCK_COUNT, ROUTE_MODULE_WALLET_FEERATE,
    ROUTE_MODULE_WALLET_PENDING_TX_CHAIN, ROUTE_MODULE_WALLET_TOTAL_VALUE,
    ROUTE_MODULE_WALLET_TX_CHAIN, ROUTE_SESSION_COUNT, ROUTE_SETUP_ADD_PEER, ROUTE_SETUP_RESTORE,
    ROUTE_SETUP_SET_LOCAL_PARAMS, ROUTE_SETUP_START_DKG, ROUTE_SETUP_STATUS, SetupAddPeerRequest,
    SetupSetLocalParamsRequest,
};
use serde::Serialize;
use serde_json::Value;
use tokio::net::UnixStream;
use tower_service::Service;

#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to the guardian's data directory (must match the daemon's
    /// `DATA_DIR`). The CLI finds the admin Unix socket at
    /// `{DATA_DIR}/cli.sock`.
    #[arg(long = "data-dir", env = "DATA_DIR")]
    data_dir: PathBuf,

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
    /// Dump full server config as JSON (use `> config.json` to save)
    Config,
    /// Number of consensus sessions this guardian has finalized
    SessionCount,
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
    /// Restore guardian config from a config file (skips DKG)
    Restore {
        /// Path to a `config.json` previously produced by `config`
        path: PathBuf,
    },
}

#[derive(Subcommand)]
enum ModuleCommands {
    /// Wallet module commands
    #[command(subcommand)]
    Wallet(WalletCommands),
    /// LN module commands
    #[command(subcommand)]
    Ln(LnCommands),
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
enum LnCommands {
    /// Gateway management
    #[command(subcommand)]
    Gateway(LnGatewayCommands),
}

#[derive(Subcommand)]
enum LnGatewayCommands {
    /// Add a vetted gateway
    Add { url: String },
    /// Remove a vetted gateway
    Remove { url: String },
    /// List vetted gateways
    List,
}

/// Tiny connector that dials a fixed Unix socket path, ignoring the URI
/// entirely. Plugs into `hyper_util::client::legacy::Client` where a TCP
/// connector would normally go.
#[derive(Clone)]
struct UnixConnector {
    path: PathBuf,
}

impl Service<hyper::Uri> for UnixConnector {
    type Response = TokioIo<UnixStream>;
    type Error = io::Error;
    type Future = Pin<Box<dyn Future<Output = io::Result<TokioIo<UnixStream>>> + Send>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, _: hyper::Uri) -> Self::Future {
        let path = self.path.clone();
        Box::pin(async move { UnixStream::connect(path).await.map(TokioIo::new) })
    }
}

async fn request<R: Serialize>(data_dir: &Path, route: &str, payload: R) -> Result<Value> {
    let socket_path = data_dir.join(CLI_SOCKET_FILENAME);
    let connector = UnixConnector {
        path: socket_path.clone(),
    };
    let client = Client::builder(TokioExecutor::new()).build(connector);

    let body_bytes = serde_json::to_vec(&payload)?;
    let uri: hyper::Uri = format!("http://localhost{route}").parse()?;
    let req = Request::post(uri)
        .header("content-type", "application/json")
        .body(Full::new(Bytes::from(body_bytes)))?;

    let resp = client.request(req).await.with_context(|| {
        format!(
            "Failed to POST {route} to guardian at {}",
            socket_path.display()
        )
    })?;

    let status = resp.status();
    let resp_bytes = resp.into_body().collect().await?.to_bytes();

    ensure!(
        status.is_success(),
        "API error ({}): {}",
        status.as_u16(),
        String::from_utf8_lossy(&resp_bytes)
    );

    if resp_bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&resp_bytes).context("Failed to parse response")
    }
}

fn print_json(value: &Value) {
    println!(
        "{}",
        serde_json::to_string_pretty(value).expect("Cannot serialize")
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = &cli.data_dir;

    let result = match cli.command {
        Commands::Invite => request(data_dir, ROUTE_INVITE, ()).await?,
        Commands::Audit => request(data_dir, ROUTE_AUDIT, ()).await?,
        Commands::Config => request(data_dir, ROUTE_CONFIG, ()).await?,
        Commands::SessionCount => request(data_dir, ROUTE_SESSION_COUNT, ()).await?,
        Commands::Setup(cmd) => match cmd {
            SetupCommands::Status => request(data_dir, ROUTE_SETUP_STATUS, ()).await?,
            SetupCommands::SetLocalParams {
                name,
                federation_name,
                federation_size,
            } => {
                request(
                    data_dir,
                    ROUTE_SETUP_SET_LOCAL_PARAMS,
                    SetupSetLocalParamsRequest {
                        name,
                        federation_name,
                        federation_size,
                    },
                )
                .await?
            }
            SetupCommands::AddPeer { setup_code } => {
                request(
                    data_dir,
                    ROUTE_SETUP_ADD_PEER,
                    SetupAddPeerRequest { setup_code },
                )
                .await?
            }
            SetupCommands::StartDkg => request(data_dir, ROUTE_SETUP_START_DKG, ()).await?,
            SetupCommands::Restore { path } => {
                let bytes = std::fs::read(&path)?;

                let cfg: Value = serde_json::from_slice(&bytes)?;

                request(data_dir, ROUTE_SETUP_RESTORE, cfg).await?
            }
        },
        Commands::Module(cmd) => match cmd {
            ModuleCommands::Wallet(cmd) => match cmd {
                WalletCommands::TotalValue => {
                    request(data_dir, ROUTE_MODULE_WALLET_TOTAL_VALUE, ()).await?
                }
                WalletCommands::BlockCount => {
                    request(data_dir, ROUTE_MODULE_WALLET_BLOCK_COUNT, ()).await?
                }
                WalletCommands::Feerate => {
                    request(data_dir, ROUTE_MODULE_WALLET_FEERATE, ()).await?
                }
                WalletCommands::PendingTxChain => {
                    request(data_dir, ROUTE_MODULE_WALLET_PENDING_TX_CHAIN, ()).await?
                }
                WalletCommands::TxChain => {
                    request(data_dir, ROUTE_MODULE_WALLET_TX_CHAIN, ()).await?
                }
            },
            ModuleCommands::Ln(cmd) => match cmd {
                LnCommands::Gateway(cmd) => match cmd {
                    LnGatewayCommands::Add { url } => {
                        request(
                            data_dir,
                            ROUTE_MODULE_LN_GATEWAY_ADD,
                            LnGatewayRequest { url },
                        )
                        .await?
                    }
                    LnGatewayCommands::Remove { url } => {
                        request(
                            data_dir,
                            ROUTE_MODULE_LN_GATEWAY_REMOVE,
                            LnGatewayRequest { url },
                        )
                        .await?
                    }
                    LnGatewayCommands::List => {
                        request(data_dir, ROUTE_MODULE_LN_GATEWAY_LIST, ()).await?
                    }
                },
            },
        },
    };

    print_json(&result);
    Ok(())
}
