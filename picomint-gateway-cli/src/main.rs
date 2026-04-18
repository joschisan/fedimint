#![deny(clippy::pedantic, clippy::nursery)]
#![allow(clippy::large_enum_variant)]

use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};

use anyhow::{Context as _, Result, ensure};
use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::PublicKey;
use clap::{Parser, Subcommand};
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::body::Bytes;
use hyper_util::client::legacy::Client;
use hyper_util::rt::{TokioExecutor, TokioIo};
use picomint_core::Amount;
use picomint_core::config::FederationId;
use picomint_gateway_cli_core::{
    CLI_SOCKET_FILENAME, FederationBalanceRequest, FederationConfigRequest, FederationJoinRequest,
    LdkChannelCloseRequest, LdkChannelOpenRequest, LdkInvoiceCreateRequest, LdkInvoicePayRequest,
    LdkOnchainSendRequest, LdkPeerConnectRequest, LdkPeerDisconnectRequest, MintCountRequest,
    MintReceiveRequest, MintSendRequest, ROUTE_FEDERATION_BALANCE, ROUTE_FEDERATION_CONFIG,
    ROUTE_FEDERATION_INVITE, ROUTE_FEDERATION_JOIN, ROUTE_FEDERATION_LIST, ROUTE_INFO,
    ROUTE_LDK_BALANCES, ROUTE_LDK_CHANNEL_CLOSE, ROUTE_LDK_CHANNEL_LIST, ROUTE_LDK_CHANNEL_OPEN,
    ROUTE_LDK_INVOICE_CREATE, ROUTE_LDK_INVOICE_PAY, ROUTE_LDK_ONCHAIN_RECEIVE,
    ROUTE_LDK_ONCHAIN_SEND, ROUTE_LDK_PEER_CONNECT, ROUTE_LDK_PEER_DISCONNECT, ROUTE_LDK_PEER_LIST,
    ROUTE_MNEMONIC, ROUTE_MODULE_MINT_COUNT, ROUTE_MODULE_MINT_RECEIVE, ROUTE_MODULE_MINT_SEND,
    ROUTE_MODULE_WALLET_INFO, ROUTE_MODULE_WALLET_RECEIVE, ROUTE_MODULE_WALLET_SEND,
    ROUTE_MODULE_WALLET_SEND_FEE, WalletInfoRequest, WalletReceiveRequest, WalletSendFeeRequest,
    WalletSendRequest,
};
use serde::Serialize;
use serde_json::Value;
use tokio::net::UnixStream;
use tower_service::Service;

#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Path to the gateway's data directory (must match the daemon's
    /// `DATA_DIR`). The CLI finds the admin Unix socket at
    /// `{DATA_DIR}/cli.sock`.
    #[arg(long = "data-dir", env = "DATA_DIR")]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Display gateway info
    Info,
    /// Display mnemonic seed words
    Mnemonic,
    /// LDK lightning node management
    #[command(subcommand)]
    Ldk(LdkCommands),
    /// Federation management
    #[command(subcommand)]
    Federation(FederationCommands),
    /// Per-federation module commands
    Module {
        /// Federation ID
        federation_id: FederationId,
        #[command(subcommand)]
        module: ModuleCommands,
    },
}

#[derive(Subcommand)]
enum LdkCommands {
    /// Get node balances
    Balances,
    /// On-chain operations
    Onchain {
        #[command(subcommand)]
        command: LdkOnchainCommands,
    },
    /// Channel operations
    Channel {
        #[command(subcommand)]
        command: LdkChannelCommands,
    },
    /// Invoice operations
    Invoice {
        #[command(subcommand)]
        command: LdkInvoiceCommands,
    },
    /// Peer management
    Peer {
        #[command(subcommand)]
        command: LdkPeerCommands,
    },
}

#[derive(Subcommand)]
enum LdkOnchainCommands {
    /// Get a receive address
    Receive,
    /// Send funds
    Send {
        #[arg(long)]
        address: bitcoin::Address<NetworkUnchecked>,
        #[arg(long)]
        amount: bitcoin::Amount,
        #[arg(long)]
        sats_per_vbyte: u64,
    },
}

#[derive(Subcommand)]
enum LdkChannelCommands {
    /// Open a channel
    Open {
        pubkey: PublicKey,
        host: String,
        channel_size_sats: u64,
        #[arg(long)]
        push_amount_sats: Option<u64>,
    },
    /// Close channels with a peer
    Close {
        pubkey: PublicKey,
        #[arg(long)]
        force: bool,
        #[arg(long, required_unless_present = "force")]
        sats_per_vbyte: Option<u64>,
    },
    /// List channels
    List,
}

#[derive(Subcommand)]
enum LdkInvoiceCommands {
    /// Create a bolt11 invoice
    Create {
        amount_msats: u64,
        #[arg(long)]
        expiry_secs: Option<u32>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Pay a bolt11 invoice
    Pay { invoice: String },
}

#[derive(Subcommand)]
enum LdkPeerCommands {
    /// Connect to a peer
    Connect { pubkey: PublicKey, host: String },
    /// Disconnect from a peer
    Disconnect { pubkey: PublicKey },
    /// List peers
    List,
}

#[derive(Subcommand)]
enum FederationCommands {
    /// Join a federation
    Join { invite: String },
    /// List connected federations
    List,
    /// Get a connected federation's JSON client config
    Config { federation_id: FederationId },
    /// Get invite code for a federation
    Invite { federation_id: FederationId },
    /// Get a federation's ecash balance
    Balance { federation_id: FederationId },
}

#[derive(Subcommand)]
enum ModuleCommands {
    /// Mint module commands
    #[command(subcommand)]
    Mint(MintCommands),
    /// Wallet module commands
    #[command(subcommand)]
    Wallet(WalletCommands),
}

#[derive(Subcommand)]
enum MintCommands {
    /// Count ecash notes by denomination
    Count,
    /// Send ecash
    Send { amount: Amount },
    /// Receive ecash
    Receive { ecash: String },
}

#[derive(Subcommand)]
enum WalletCommands {
    /// Query wallet info
    Info { subcommand: String },
    /// Get send fee estimate
    SendFee,
    /// Send onchain from federation wallet
    Send {
        address: bitcoin::Address<NetworkUnchecked>,
        amount: bitcoin::Amount,
        #[arg(long)]
        fee: Option<bitcoin::Amount>,
    },
    /// Get receive address
    Receive,
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
            "Failed to POST {route} to gateway at {}",
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
        serde_json::from_slice(&resp_bytes).context("Failed to parse gateway response")
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
    let d = &cli.data_dir;

    let result = match cli.command {
        Commands::Info => request(d, ROUTE_INFO, ()).await?,
        Commands::Mnemonic => request(d, ROUTE_MNEMONIC, ()).await?,
        Commands::Ldk(cmd) => match cmd {
            LdkCommands::Balances => request(d, ROUTE_LDK_BALANCES, ()).await?,
            LdkCommands::Onchain { command } => match command {
                LdkOnchainCommands::Receive => request(d, ROUTE_LDK_ONCHAIN_RECEIVE, ()).await?,
                LdkOnchainCommands::Send {
                    address,
                    amount,
                    sats_per_vbyte,
                } => {
                    request(
                        d,
                        ROUTE_LDK_ONCHAIN_SEND,
                        LdkOnchainSendRequest {
                            address,
                            amount,
                            sats_per_vbyte,
                        },
                    )
                    .await?
                }
            },
            LdkCommands::Channel { command } => match command {
                LdkChannelCommands::Open {
                    pubkey,
                    host,
                    channel_size_sats,
                    push_amount_sats,
                } => {
                    request(
                        d,
                        ROUTE_LDK_CHANNEL_OPEN,
                        LdkChannelOpenRequest {
                            pubkey,
                            host,
                            channel_size_sats,
                            push_amount_sats: push_amount_sats.unwrap_or(0),
                        },
                    )
                    .await?
                }
                LdkChannelCommands::Close {
                    pubkey,
                    force,
                    sats_per_vbyte,
                } => {
                    request(
                        d,
                        ROUTE_LDK_CHANNEL_CLOSE,
                        LdkChannelCloseRequest {
                            pubkey,
                            force,
                            sats_per_vbyte,
                        },
                    )
                    .await?
                }
                LdkChannelCommands::List => request(d, ROUTE_LDK_CHANNEL_LIST, ()).await?,
            },
            LdkCommands::Invoice { command } => match command {
                LdkInvoiceCommands::Create {
                    amount_msats,
                    expiry_secs,
                    description,
                } => {
                    request(
                        d,
                        ROUTE_LDK_INVOICE_CREATE,
                        LdkInvoiceCreateRequest {
                            amount_msats,
                            expiry_secs,
                            description,
                        },
                    )
                    .await?
                }
                LdkInvoiceCommands::Pay { invoice } => {
                    let invoice: lightning_invoice::Bolt11Invoice =
                        invoice.parse().context("Invalid bolt11 invoice")?;
                    request(d, ROUTE_LDK_INVOICE_PAY, LdkInvoicePayRequest { invoice }).await?
                }
            },
            LdkCommands::Peer { command } => match command {
                LdkPeerCommands::Connect { pubkey, host } => {
                    request(
                        d,
                        ROUTE_LDK_PEER_CONNECT,
                        LdkPeerConnectRequest { pubkey, host },
                    )
                    .await?
                }
                LdkPeerCommands::Disconnect { pubkey } => {
                    request(
                        d,
                        ROUTE_LDK_PEER_DISCONNECT,
                        LdkPeerDisconnectRequest { pubkey },
                    )
                    .await?
                }
                LdkPeerCommands::List => request(d, ROUTE_LDK_PEER_LIST, ()).await?,
            },
        },

        Commands::Federation(cmd) => match cmd {
            FederationCommands::Join { invite } => {
                request(d, ROUTE_FEDERATION_JOIN, FederationJoinRequest { invite }).await?
            }
            FederationCommands::List => request(d, ROUTE_FEDERATION_LIST, ()).await?,
            FederationCommands::Config { federation_id } => {
                request(
                    d,
                    ROUTE_FEDERATION_CONFIG,
                    FederationConfigRequest {
                        federation_id: Some(federation_id),
                    },
                )
                .await?
            }
            FederationCommands::Invite { federation_id } => {
                request(
                    d,
                    ROUTE_FEDERATION_INVITE,
                    serde_json::json!({ "federation_id": federation_id }),
                )
                .await?
            }
            FederationCommands::Balance { federation_id } => {
                request(
                    d,
                    ROUTE_FEDERATION_BALANCE,
                    FederationBalanceRequest { federation_id },
                )
                .await?
            }
        },

        Commands::Module {
            federation_id,
            module,
        } => match module {
            ModuleCommands::Mint(cmd) => match cmd {
                MintCommands::Count => {
                    request(
                        d,
                        ROUTE_MODULE_MINT_COUNT,
                        MintCountRequest { federation_id },
                    )
                    .await?
                }
                MintCommands::Send { amount } => {
                    request(
                        d,
                        ROUTE_MODULE_MINT_SEND,
                        MintSendRequest {
                            federation_id,
                            amount,
                        },
                    )
                    .await?
                }
                MintCommands::Receive { ecash } => {
                    request(
                        d,
                        ROUTE_MODULE_MINT_RECEIVE,
                        MintReceiveRequest { notes: ecash },
                    )
                    .await?
                }
            },
            ModuleCommands::Wallet(cmd) => match cmd {
                WalletCommands::Info { subcommand } => {
                    request(
                        d,
                        ROUTE_MODULE_WALLET_INFO,
                        WalletInfoRequest {
                            federation_id,
                            subcommand,
                        },
                    )
                    .await?
                }
                WalletCommands::SendFee => {
                    request(
                        d,
                        ROUTE_MODULE_WALLET_SEND_FEE,
                        WalletSendFeeRequest { federation_id },
                    )
                    .await?
                }
                WalletCommands::Send {
                    address,
                    amount,
                    fee,
                } => {
                    request(
                        d,
                        ROUTE_MODULE_WALLET_SEND,
                        WalletSendRequest {
                            federation_id,
                            address,
                            amount,
                            fee,
                        },
                    )
                    .await?
                }
                WalletCommands::Receive => {
                    request(
                        d,
                        ROUTE_MODULE_WALLET_RECEIVE,
                        WalletReceiveRequest { federation_id },
                    )
                    .await?
                }
            },
        },
    };

    print_json(&result);
    Ok(())
}
