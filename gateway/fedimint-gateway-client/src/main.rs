#![deny(clippy::pedantic, clippy::nursery)]
#![allow(clippy::large_enum_variant)]

use anyhow::{Context, Result, ensure};
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256;
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
    GetBalances,
    /// Shutdown the gateway
    Stop,
    /// Display mnemonic seed words
    Seed,
    /// LDK lightning node management
    #[command(subcommand)]
    Lightning(LightningCommands),
    /// Ecash management
    #[command(subcommand)]
    Ecash(EcashCommands),
    /// On-chain wallet management
    #[command(subcommand)]
    Onchain(OnchainCommands),
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
enum LightningCommands {
    /// Create a bolt11 invoice
    CreateInvoice {
        amount_msats: u64,
        #[arg(long)]
        expiry_secs: Option<u32>,
        #[arg(long)]
        description: Option<String>,
    },
    /// Pay a bolt11 invoice
    PayInvoice { invoice: String },
    /// Open a channel with another lightning node
    OpenChannel {
        #[arg(long)]
        pubkey: bitcoin::secp256k1::PublicKey,
        #[arg(long)]
        host: String,
        #[arg(long)]
        channel_size_sats: u64,
        #[arg(long)]
        push_amount_sats: Option<u64>,
    },
    /// Close channels with a peer
    CloseChannelsWithPeer {
        #[arg(long)]
        pubkey: bitcoin::secp256k1::PublicKey,
        #[arg(long)]
        force: bool,
        #[arg(long, required_unless_present = "force")]
        sats_per_vbyte: Option<u64>,
    },
    /// List channels
    ListChannels,
    /// List lightning transactions
    ListTransactions {
        #[arg(long)]
        start_secs: u64,
        #[arg(long)]
        end_secs: u64,
    },
    /// Get details about a specific invoice
    GetInvoice {
        #[arg(long)]
        payment_hash: sha256::Hash,
    },
}

#[derive(Subcommand)]
enum EcashCommands {
    /// Generate a new peg-in address
    Pegin {
        #[arg(long)]
        federation_id: FederationId,
    },
    /// Trigger a recheck for deposits on a deposit address
    PeginRecheck {
        #[arg(long)]
        address: bitcoin::Address<NetworkUnchecked>,
        #[arg(long)]
        federation_id: FederationId,
    },
    /// Send funds from the gateway's onchain wallet to the federation's ecash
    /// wallet
    PeginFromOnchain {
        #[arg(long)]
        federation_id: FederationId,
        #[arg(long)]
        amount: BitcoinAmountOrAll,
        #[arg(long)]
        fee_rate_sats_per_vbyte: u64,
    },
    /// Claim funds from a gateway federation to an on-chain address
    Pegout {
        #[arg(long)]
        federation_id: FederationId,
        #[arg(long)]
        amount: BitcoinAmountOrAll,
        #[arg(long)]
        address: bitcoin::Address<NetworkUnchecked>,
    },
    /// Claim funds from a gateway federation to the gateway's onchain wallet
    PegoutToOnchain {
        #[arg(long)]
        federation_id: FederationId,
        #[arg(long)]
        amount: BitcoinAmountOrAll,
    },
    /// Send e-cash out of band
    Send {
        #[arg(long)]
        federation_id: FederationId,
        amount: Amount,
    },
    /// Receive e-cash out of band
    Receive {
        #[arg(long)]
        notes: String,
        #[arg(long = "no-wait", action = clap::ArgAction::SetFalse)]
        wait: bool,
    },
}

#[derive(Subcommand)]
enum OnchainCommands {
    /// Get a Bitcoin address from the gateway's lightning node's onchain wallet
    Address,
    /// Send funds from the lightning node's on-chain wallet
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
        #[arg(long)]
        federation_id: Option<FederationId>,
        #[arg(long)]
        ln_base: Option<Amount>,
        #[arg(long)]
        ln_ppm: Option<u64>,
        #[arg(long)]
        tx_base: Option<Amount>,
        #[arg(long)]
        tx_ppm: Option<u64>,
    },
    /// Gets each connected federation's JSON client config
    ClientConfig {
        #[arg(long)]
        federation_id: Option<FederationId>,
    },
    /// Get invite codes for each federation
    InviteCodes,
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
        Commands::GetBalances => request_get(addr, ROUTE_BALANCES)?,
        Commands::Stop => request_get(addr, ROUTE_STOP)?,
        Commands::Seed => request_get(addr, ROUTE_MNEMONIC)?,

        Commands::Lightning(cmd) => match cmd {
            LightningCommands::CreateInvoice {
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
            LightningCommands::PayInvoice { invoice } => {
                let invoice: lightning_invoice::Bolt11Invoice =
                    invoice.parse().context("Invalid bolt11 invoice")?;
                request(
                    addr,
                    ROUTE_LDK_INVOICE_PAY,
                    PayInvoiceForOperatorPayload { invoice },
                )?
            }
            LightningCommands::OpenChannel {
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
            LightningCommands::CloseChannelsWithPeer {
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
            LightningCommands::ListChannels => request_get(addr, ROUTE_LDK_CHANNEL_LIST)?,
            LightningCommands::ListTransactions {
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
            LightningCommands::GetInvoice { payment_hash } => request(
                addr,
                ROUTE_LDK_INVOICE_GET,
                GetInvoiceRequest { payment_hash },
            )?,
        },

        Commands::Ecash(cmd) => match cmd {
            EcashCommands::Pegin { federation_id } => request(
                addr,
                ROUTE_ECASH_PEGIN,
                DepositAddressPayload { federation_id },
            )?,
            EcashCommands::PeginRecheck {
                address,
                federation_id,
            } => request(
                addr,
                ROUTE_ECASH_PEGIN_RECHECK,
                DepositAddressRecheckPayload {
                    address,
                    federation_id,
                },
            )?,
            EcashCommands::PeginFromOnchain {
                federation_id,
                amount,
                fee_rate_sats_per_vbyte,
            } => request(
                addr,
                ROUTE_ECASH_PEGIN_FROM_ONCHAIN,
                PeginFromOnchainPayload {
                    federation_id,
                    amount,
                    fee_rate_sats_per_vbyte,
                },
            )?,
            EcashCommands::Pegout {
                federation_id,
                amount,
                address,
            } => request(
                addr,
                ROUTE_ECASH_PEGOUT,
                WithdrawPayload {
                    federation_id,
                    amount,
                    address,
                    quoted_fees: None,
                },
            )?,
            EcashCommands::PegoutToOnchain {
                federation_id,
                amount,
            } => request(
                addr,
                ROUTE_ECASH_PEGOUT_TO_ONCHAIN,
                WithdrawToOnchainPayload {
                    federation_id,
                    amount,
                },
            )?,
            EcashCommands::Send {
                federation_id,
                amount,
            } => request(
                addr,
                ROUTE_ECASH_SEND,
                SpendEcashPayload {
                    federation_id,
                    amount,
                },
            )?,
            EcashCommands::Receive { notes, wait } => request(
                addr,
                ROUTE_ECASH_RECEIVE,
                ReceiveEcashPayload { notes, wait },
            )?,
        },

        Commands::Onchain(cmd) => match cmd {
            OnchainCommands::Address => request_get(addr, ROUTE_LDK_ONCHAIN_RECEIVE)?,
            OnchainCommands::Send {
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
                    federation_id,
                    lightning_base: ln_base,
                    lightning_parts_per_million: ln_ppm,
                    transaction_base: tx_base,
                    transaction_parts_per_million: tx_ppm,
                },
            )?,
            FederationCommands::ClientConfig { federation_id } => {
                request(addr, ROUTE_FED_CONFIG, ConfigPayload { federation_id })?
            }
            FederationCommands::InviteCodes => request_get(addr, ROUTE_FED_INVITE)?,
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
