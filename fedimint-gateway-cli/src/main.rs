#![deny(clippy::pedantic, clippy::nursery)]
#![allow(clippy::large_enum_variant)]

use anyhow::{Context, Result, ensure};
use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::PublicKey;
use clap::{Parser, Subcommand};
use fedimint_core::config::FederationId;
use fedimint_core::{Amount, BitcoinAmountOrAll};
use fedimint_gateway_common::*;
use serde::Serialize;
use serde_json::Value;

#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Gateway admin API address
    #[arg(
        short,
        long,
        env = "FM_GATEWAY_ADDR",
        default_value = "http://127.0.0.1:80"
    )]
    address: String,

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
    /// Transaction operations
    Transaction {
        #[command(subcommand)]
        command: LdkTransactionCommands,
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
        amount: BitcoinAmountOrAll,
        #[arg(long)]
        fee_rate_sats_per_vbyte: u64,
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
enum LdkTransactionCommands {
    /// List transactions
    List {
        #[arg(long)]
        start_secs: u64,
        #[arg(long)]
        end_secs: u64,
    },
}

#[derive(Subcommand)]
enum FederationCommands {
    /// Join a federation
    Join {
        invite_code: String,
        #[arg(long)]
        recover: Option<bool>,
    },
    /// List connected federations
    List,
    /// Set routing fees
    SetFees {
        federation_id: FederationId,
        #[arg(long)]
        ln_base: Option<Amount>,
        #[arg(long)]
        ln_ppm: Option<u64>,
        #[arg(long)]
        tx_base: Option<Amount>,
        #[arg(long)]
        tx_ppm: Option<u64>,
    },
    /// Get a connected federation's JSON client config
    Config { federation_id: FederationId },
    /// Get invite code for a federation
    Invite { federation_id: FederationId },
}

#[derive(Subcommand)]
enum ModuleCommands {
    /// Mint module commands
    #[command(subcommand)]
    Mintv2(MintCommands),
    /// Wallet module commands
    #[command(subcommand)]
    Walletv2(WalletCommands),
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

fn request<R: Serialize>(addr: &str, route: &str, payload: R) -> Result<Value> {
    let response = reqwest::blocking::Client::new()
        .post(format!("{addr}{route}"))
        .json(&serde_json::to_value(payload)?)
        .send()
        .context("Failed to connect to gateway")?;

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
        serde_json::from_str(&text).context("Failed to parse gateway response")
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
        Commands::Info => request(addr, ROUTE_INFO, ())?,
        Commands::Mnemonic => request(addr, ROUTE_MNEMONIC, ())?,
        Commands::Ldk(cmd) => match cmd {
            LdkCommands::Balances => request(addr, ROUTE_LDK_BALANCES, ())?,
            LdkCommands::Onchain { command } => match command {
                LdkOnchainCommands::Receive => request(addr, ROUTE_LDK_ONCHAIN_RECEIVE, ())?,
                LdkOnchainCommands::Send {
                    address,
                    amount,
                    fee_rate_sats_per_vbyte,
                } => request(
                    addr,
                    ROUTE_LDK_ONCHAIN_SEND,
                    SendOnchainRequest {
                        address,
                        amount,
                        fee_rate_sats_per_vbyte,
                    },
                )?,
            },
            LdkCommands::Channel { command } => match command {
                LdkChannelCommands::Open {
                    pubkey,
                    host,
                    channel_size_sats,
                    push_amount_sats,
                } => request(
                    addr,
                    ROUTE_LDK_CHANNEL_OPEN,
                    OpenChannelRequest {
                        pubkey,
                        host,
                        channel_size_sats,
                        push_amount_sats: push_amount_sats.unwrap_or(0),
                    },
                )?,
                LdkChannelCommands::Close {
                    pubkey,
                    force,
                    sats_per_vbyte,
                } => request(
                    addr,
                    ROUTE_LDK_CHANNEL_CLOSE,
                    CloseChannelsWithPeerRequest {
                        pubkey,
                        force,
                        sats_per_vbyte,
                    },
                )?,
                LdkChannelCommands::List => request(addr, ROUTE_LDK_CHANNEL_LIST, ())?,
            },
            LdkCommands::Invoice { command } => match command {
                LdkInvoiceCommands::Create {
                    amount_msats,
                    expiry_secs,
                    description,
                } => request(
                    addr,
                    ROUTE_LDK_INVOICE_CREATE,
                    CreateInvoiceForOperatorPayload {
                        amount_msats,
                        expiry_secs,
                        description,
                    },
                )?,
                LdkInvoiceCommands::Pay { invoice } => {
                    let invoice: lightning_invoice::Bolt11Invoice =
                        invoice.parse().context("Invalid bolt11 invoice")?;
                    request(
                        addr,
                        ROUTE_LDK_INVOICE_PAY,
                        PayInvoiceForOperatorPayload { invoice },
                    )?
                }
            },
            LdkCommands::Peer { command } => match command {
                LdkPeerCommands::Connect { pubkey, host } => request(
                    addr,
                    ROUTE_LDK_PEER_CONNECT,
                    PeerConnectRequest { pubkey, host },
                )?,
                LdkPeerCommands::Disconnect { pubkey } => request(
                    addr,
                    ROUTE_LDK_PEER_DISCONNECT,
                    PeerDisconnectRequest { pubkey },
                )?,
                LdkPeerCommands::List => request(addr, ROUTE_LDK_PEER_LIST, ())?,
            },
            LdkCommands::Transaction { command } => match command {
                LdkTransactionCommands::List {
                    start_secs,
                    end_secs,
                } => request(
                    addr,
                    ROUTE_LDK_TRANSACTION_LIST,
                    ListTransactionsPayload {
                        start_secs,
                        end_secs,
                    },
                )?,
            },
        },

        Commands::Federation(cmd) => match cmd {
            FederationCommands::Join {
                invite_code,
                recover,
            } => request(
                addr,
                ROUTE_FED_JOIN,
                ConnectFedPayload {
                    invite_code,
                    recover,
                },
            )?,
            FederationCommands::List => request(addr, ROUTE_FED_LIST, ())?,
            FederationCommands::SetFees {
                federation_id,
                ln_base,
                ln_ppm,
                tx_base,
                tx_ppm,
            } => request(
                addr,
                ROUTE_FED_SET_FEES,
                SetFeesPayload {
                    federation_id: Some(federation_id),
                    ln_base,
                    ln_ppm,
                    tx_base,
                    tx_ppm,
                },
            )?,
            FederationCommands::Config { federation_id } => request(
                addr,
                ROUTE_FED_CONFIG,
                ConfigPayload {
                    federation_id: Some(federation_id),
                },
            )?,
            FederationCommands::Invite { federation_id } => request(
                addr,
                ROUTE_FED_INVITE,
                serde_json::json!({ "federation_id": federation_id }),
            )?,
        },

        Commands::Module {
            federation_id,
            module,
        } => match module {
            ModuleCommands::Mintv2(cmd) => match cmd {
                MintCommands::Count => request(
                    addr,
                    ROUTE_MODULE_MINT_COUNT,
                    ModuleMintCountRequest { federation_id },
                )?,
                MintCommands::Send { amount } => request(
                    addr,
                    ROUTE_MODULE_MINT_SEND,
                    ModuleMintSendRequest {
                        federation_id,
                        amount,
                    },
                )?,
                MintCommands::Receive { ecash } => request(
                    addr,
                    ROUTE_MODULE_MINT_RECEIVE,
                    ModuleMintReceiveRequest {
                        federation_id,
                        ecash,
                    },
                )?,
            },
            ModuleCommands::Walletv2(cmd) => match cmd {
                WalletCommands::Info { subcommand } => request(
                    addr,
                    ROUTE_MODULE_WALLET_INFO,
                    ModuleWalletInfoRequest {
                        federation_id,
                        subcommand,
                    },
                )?,
                WalletCommands::SendFee => request(
                    addr,
                    ROUTE_MODULE_WALLET_SEND_FEE,
                    ModuleWalletSendFeeRequest { federation_id },
                )?,
                WalletCommands::Send {
                    address,
                    amount,
                    fee,
                } => request(
                    addr,
                    ROUTE_MODULE_WALLET_SEND,
                    ModuleWalletSendRequest {
                        federation_id,
                        address,
                        amount,
                        fee,
                    },
                )?,
                WalletCommands::Receive => request(
                    addr,
                    ROUTE_MODULE_WALLET_RECEIVE,
                    ModuleWalletReceiveRequest { federation_id },
                )?,
            },
        },
    };

    print_json(&result);
    Ok(())
}
