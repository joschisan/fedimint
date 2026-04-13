use std::collections::BTreeMap;

use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256;
use fedimint_core::config::{FederationId, JsonClientConfig};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::{Amount, BitcoinAmountOrAll, PeerId, secp256k1};
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};

// Top-level
pub const ROUTE_INFO: &str = "/info";
pub const ROUTE_MNEMONIC: &str = "/mnemonic";

// LDK node management
pub const ROUTE_LDK_BALANCES: &str = "/ldk/balances";
pub const ROUTE_LDK_CHANNEL_OPEN: &str = "/ldk/channel/open";
pub const ROUTE_LDK_CHANNEL_CLOSE: &str = "/ldk/channel/close";
pub const ROUTE_LDK_CHANNEL_LIST: &str = "/ldk/channel/list";
pub const ROUTE_LDK_ONCHAIN_RECEIVE: &str = "/ldk/onchain/receive";
pub const ROUTE_LDK_ONCHAIN_SEND: &str = "/ldk/onchain/send";
pub const ROUTE_LDK_INVOICE_CREATE: &str = "/ldk/invoice/create";
pub const ROUTE_LDK_INVOICE_PAY: &str = "/ldk/invoice/pay";
pub const ROUTE_LDK_PEER_CONNECT: &str = "/ldk/peer/connect";
pub const ROUTE_LDK_PEER_DISCONNECT: &str = "/ldk/peer/disconnect";
pub const ROUTE_LDK_PEER_LIST: &str = "/ldk/peer/list";
pub const ROUTE_LDK_TRANSACTION_LIST: &str = "/ldk/transaction/list";

// Federation management
pub const ROUTE_FEDERATION_JOIN: &str = "/federation/join";
pub const ROUTE_FEDERATION_LIST: &str = "/federation/list";
pub const ROUTE_FEDERATION_CONFIG: &str = "/federation/config";
pub const ROUTE_FEDERATION_INVITE: &str = "/federation/invite";

// Per-federation module commands
pub const ROUTE_MODULE_MINT_COUNT: &str = "/module/mintv2/count";
pub const ROUTE_MODULE_MINT_SEND: &str = "/module/mintv2/send";
pub const ROUTE_MODULE_MINT_RECEIVE: &str = "/module/mintv2/receive";
pub const ROUTE_MODULE_WALLET_INFO: &str = "/module/walletv2/info";
pub const ROUTE_MODULE_WALLET_SEND_FEE: &str = "/module/walletv2/send-fee";
pub const ROUTE_MODULE_WALLET_SEND: &str = "/module/walletv2/send";
pub const ROUTE_MODULE_WALLET_RECEIVE: &str = "/module/walletv2/receive";

// --- /info ---

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct InfoResponse {
    pub public_key: secp256k1::PublicKey,
    pub alias: String,
    pub network: String,
    pub block_height: u64,
    pub synced_to_chain: bool,
}

// --- /mnemonic ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MnemonicResponse {
    pub mnemonic: Vec<String>,
}

// --- /ldk/balances ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkBalancesResponse {
    pub onchain_balance_sats: u64,
    pub lightning_balance_msats: u64,
    pub ecash_balances: Vec<FederationBalanceInfo>,
    pub inbound_lightning_liquidity_msats: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationBalanceInfo {
    pub federation_id: FederationId,
    pub ecash_balance_msats: Amount,
}

// --- /ldk/channel/open ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkChannelOpenRequest {
    pub pubkey: secp256k1::PublicKey,
    pub host: String,
    pub channel_size_sats: u64,
    pub push_amount_sats: u64,
}

// --- /ldk/channel/close ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkChannelCloseRequest {
    pub pubkey: secp256k1::PublicKey,
    #[serde(default)]
    pub force: bool,
    pub sats_per_vbyte: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkChannelCloseResponse {
    pub num_channels_closed: u32,
}

// --- /ldk/channel/list ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkChannelListResponse {
    pub channels: Vec<ChannelInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ChannelInfo {
    pub remote_pubkey: secp256k1::PublicKey,
    pub remote_alias: Option<String>,
    pub remote_address: Option<String>,
    pub channel_size_sats: u64,
    pub outbound_liquidity_sats: u64,
    pub inbound_liquidity_sats: u64,
    pub is_usable: bool,
    pub is_outbound: bool,
    pub funding_txid: Option<bitcoin::Txid>,
}

// --- /ldk/onchain/receive ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkOnchainReceiveResponse {
    pub address: String,
}

// --- /ldk/onchain/send ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkOnchainSendRequest {
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub amount: BitcoinAmountOrAll,
    pub fee_rate_sats_per_vbyte: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkOnchainSendResponse {
    pub txid: bitcoin::Txid,
}

// --- /ldk/invoice/create ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkInvoiceCreateRequest {
    pub amount_msats: u64,
    pub expiry_secs: Option<u32>,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkInvoiceCreateResponse {
    pub invoice: String,
}

// --- /ldk/invoice/pay ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkInvoicePayRequest {
    pub invoice: Bolt11Invoice,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkInvoicePayResponse {
    pub preimage: String,
}

// --- /ldk/peer/connect ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkPeerConnectRequest {
    pub pubkey: secp256k1::PublicKey,
    pub host: String,
}

// --- /ldk/peer/disconnect ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkPeerDisconnectRequest {
    pub pubkey: secp256k1::PublicKey,
}

// --- /ldk/peer/list ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkPeerListResponse {
    pub peers: Vec<PeerInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PeerInfo {
    pub node_id: secp256k1::PublicKey,
    pub address: String,
    pub is_connected: bool,
}

// --- /ldk/transaction/list ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkTransactionListRequest {
    pub start_secs: u64,
    pub end_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkTransactionListResponse {
    pub transactions: Vec<PaymentDetails>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PaymentDetails {
    pub payment_hash: Option<sha256::Hash>,
    pub preimage: Option<String>,
    pub payment_kind: PaymentKind,
    pub amount: Amount,
    pub direction: PaymentDirection,
    pub status: PaymentStatus,
    pub timestamp_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum PaymentKind {
    Bolt11,
    Bolt12Offer,
    Bolt12Refund,
    Onchain,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum PaymentDirection {
    Outbound,
    Inbound,
}

#[derive(Debug, Serialize, Deserialize, Clone, Eq, PartialEq)]
pub enum PaymentStatus {
    Pending,
    Succeeded,
    Failed,
}

// --- /federation/join ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationJoinRequest {
    pub invite_code: String,
    pub recover: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FederationInfo {
    pub federation_id: FederationId,
    pub federation_name: Option<String>,
    pub balance_msat: Amount,
}

// --- /federation/list ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationListResponse {
    pub federations: Vec<FederationInfo>,
}

// --- /federation/config ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationConfigRequest {
    pub federation_id: Option<FederationId>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct FederationConfigResponse {
    pub federations: BTreeMap<FederationId, JsonClientConfig>,
}

// --- /federation/invite ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationInviteResponse {
    pub invite_codes: BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>>,
}

// --- /module/mintv2/count ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintCountRequest {
    pub federation_id: FederationId,
}

// --- /module/mintv2/send ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintSendRequest {
    pub federation_id: FederationId,
    pub amount: Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintSendResponse {
    pub notes: String,
}

// --- /module/mintv2/receive ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintReceiveRequest {
    pub notes: String,
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintReceiveResponse {
    pub amount: Amount,
}

// --- /module/walletv2/info ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletInfoRequest {
    pub federation_id: FederationId,
    pub subcommand: String,
}

// --- /module/walletv2/send-fee ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletSendFeeRequest {
    pub federation_id: FederationId,
}

// --- /module/walletv2/send ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletSendRequest {
    pub federation_id: FederationId,
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub amount: bitcoin::Amount,
    pub fee: Option<bitcoin::Amount>,
}

// --- /module/walletv2/receive ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletReceiveRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletReceiveResponse {
    pub address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
}
