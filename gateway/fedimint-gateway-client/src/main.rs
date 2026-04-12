#![deny(clippy::pedantic, clippy::nursery)]
#![allow(clippy::large_enum_variant)]

use anyhow::{Context, Result, ensure};
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256;
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
    /// Display gateway balances
    Balances,
    /// Shutdown the gateway
    Stop,
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
    /// Create a bolt11 invoice
    InvoiceCreate {
        amount_msats: u64,
        #[arg(long)]
        expiry_secs: Option<u32>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Pay a bolt11 invoice
    InvoicePay { invoice: String },
    /// Get details about a specific invoice
    InvoiceGet {
        #[arg(long)]
        payment_hash: sha256::Hash,
    },
    /// Open a channel with another lightning node
    ChannelOpen {
        pubkey: PublicKey,
        host: String,
        channel_size_sats: u64,
        #[arg(long)]
        push_amount_sats: Option<u64>,
    },
    /// Close channels with a peer
    ChannelClose {
        pubkey: PublicKey,
        #[arg(long)]
        force: bool,
        #[arg(long, required_unless_present = "force")]
        sats_per_vbyte: Option<u64>,
    },
    /// List channels
    ChannelList,
    /// List lightning transactions
    TransactionList {
        #[arg(long)]
        start_secs: u64,
        #[arg(long)]
        end_secs: u64,
    },
    /// Get a Bitcoin address from the gateway's lightning node's onchain wallet
    OnchainAddress,
    /// Send funds from the lightning node's on-chain wallet
    OnchainSend {
        #[arg(long)]
        address: bitcoin::Address<NetworkUnchecked>,
        #[arg(long)]
        amount: BitcoinAmountOrAll,
        #[arg(long)]
        fee_rate_sats_per_vbyte: u64,
    },
    /// Display LDK balances
    Balances,
    /// Connect to a peer
    PeerConnect { pubkey: PublicKey, host: String },
    /// Disconnect from a peer
    PeerDisconnect { pubkey: PublicKey },
    /// List connected peers
    PeerList,
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

fn request_get(addr: &str, route: &str) -> Result<Value> {
    let response = reqwest::blocking::Client::new()
        .get(format!("{addr}{route}"))
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
        Commands::Info => request_get(addr, ROUTE_INFO)?,
        Commands::Balances => request_get(addr, ROUTE_BALANCES)?,
        Commands::Stop => request_get(addr, ROUTE_STOP)?,
        Commands::Mnemonic => request_get(addr, ROUTE_MNEMONIC)?,

        Commands::Ldk(cmd) => match cmd {
            LdkCommands::InvoiceCreate {
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
            LdkCommands::InvoicePay { invoice } => {
                let invoice: lightning_invoice::Bolt11Invoice =
                    invoice.parse().context("Invalid bolt11 invoice")?;
                request(
                    addr,
                    ROUTE_LDK_INVOICE_PAY,
                    PayInvoiceForOperatorPayload { invoice },
                )?
            }
            LdkCommands::InvoiceGet { payment_hash } => request(
                addr,
                ROUTE_LDK_INVOICE_GET,
                GetInvoiceRequest { payment_hash },
            )?,
            LdkCommands::ChannelOpen {
                pubkey,
                host,
                channel_size_sats,
                push_amount_sats,
            } => {
                let payload = OpenChannelRequest {
                    pubkey,
                    host,
                    channel_size_sats,
                    push_amount_sats: push_amount_sats.unwrap_or(0),
                };
                request(addr, ROUTE_LDK_CHANNEL_OPEN, payload)?
            }
            LdkCommands::ChannelClose {
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
            LdkCommands::ChannelList => request_get(addr, ROUTE_LDK_CHANNEL_LIST)?,
            LdkCommands::TransactionList {
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
            LdkCommands::OnchainAddress => request_get(addr, ROUTE_LDK_ONCHAIN_RECEIVE)?,
            LdkCommands::OnchainSend {
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
            LdkCommands::Balances => request_get(addr, ROUTE_LDK_BALANCES)?,
            LdkCommands::PeerConnect { pubkey, host } => request(
                addr,
                ROUTE_LDK_PEER_CONNECT,
                PeerConnectRequest { pubkey, host },
            )?,
            LdkCommands::PeerDisconnect { pubkey } => request(
                addr,
                ROUTE_LDK_PEER_DISCONNECT,
                PeerDisconnectRequest { pubkey },
            )?,
            LdkCommands::PeerList => request_get(addr, ROUTE_LDK_PEER_LIST)?,
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
                    use_tor: None,
                    recover,
                },
            )?,
            FederationCommands::List => request_get(addr, ROUTE_FED_LIST)?,
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
                    lightning_base: ln_base,
                    lightning_parts_per_million: ln_ppm,
                    transaction_base: tx_base,
                    transaction_parts_per_million: tx_ppm,
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
