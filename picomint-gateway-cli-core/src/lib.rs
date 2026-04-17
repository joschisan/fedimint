use std::collections::BTreeMap;

use bitcoin::address::NetworkUnchecked;
use lightning_invoice::Bolt11Invoice;
use picomint_core::config::FederationId;
use picomint_core::invite_code::InviteCode;
use picomint_core::{Amount, BitcoinAmountOrAll, PeerId, secp256k1};
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

// Federation management
pub const ROUTE_FEDERATION_JOIN: &str = "/federation/join";
pub const ROUTE_FEDERATION_LIST: &str = "/federation/list";
pub const ROUTE_FEDERATION_CONFIG: &str = "/federation/config";
pub const ROUTE_FEDERATION_INVITE: &str = "/federation/invite";
pub const ROUTE_FEDERATION_BALANCE: &str = "/federation/balance";

// Per-federation module commands
pub const ROUTE_MODULE_MINT_COUNT: &str = "/module/mint/count";
pub const ROUTE_MODULE_MINT_SEND: &str = "/module/mint/send";
pub const ROUTE_MODULE_MINT_RECEIVE: &str = "/module/mint/receive";
pub const ROUTE_MODULE_WALLET_INFO: &str = "/module/wallet/info";
pub const ROUTE_MODULE_WALLET_SEND_FEE: &str = "/module/wallet/send-fee";
pub const ROUTE_MODULE_WALLET_SEND: &str = "/module/wallet/send";
pub const ROUTE_MODULE_WALLET_RECEIVE: &str = "/module/wallet/receive";

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkBalancesResponse {
    /// The total balance in the on-chain wallet
    pub total_onchain_balance_sats: u64,
    /// The total inbound capacity across all channels
    pub total_inbound_capacity_msat: u64,
    /// The total outbound capacity across all channels
    pub total_outbound_capacity_msat: u64,
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
    pub address: bitcoin::Address<NetworkUnchecked>,
}

// --- /ldk/onchain/send ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct LdkOnchainSendRequest {
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub amount: BitcoinAmountOrAll,
    pub sats_per_vbyte: u64,
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

// --- /federation/join ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationJoinRequest {
    pub invite: String,
}

// --- /federation/balance ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationBalanceRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationBalanceResponse {
    pub balance_msat: Amount,
}

// --- /federation/list ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationListResponse {
    pub federations: Vec<FederationInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FederationInfo {
    pub federation_id: FederationId,
    pub federation_name: Option<String>,
}

// --- /federation/config ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationConfigRequest {
    pub federation_id: Option<FederationId>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct FederationConfigResponse {
    pub federations: BTreeMap<FederationId, serde_json::Value>,
}

// --- /federation/invite ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct FederationInviteResponse {
    pub invite_codes: BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>>,
}

// --- /module/mint/count ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintCountRequest {
    pub federation_id: FederationId,
}

// --- /module/mint/send ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintSendRequest {
    pub federation_id: FederationId,
    pub amount: Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintSendResponse {
    pub notes: String,
}

// --- /module/mint/receive ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintReceiveRequest {
    pub notes: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MintReceiveResponse {
    pub amount: Amount,
}

// --- /module/wallet/info ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletInfoRequest {
    pub federation_id: FederationId,
    pub subcommand: String,
}

// --- /module/wallet/send-fee ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletSendFeeRequest {
    pub federation_id: FederationId,
}

// --- /module/wallet/send ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletSendRequest {
    pub federation_id: FederationId,
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub amount: bitcoin::Amount,
    pub fee: Option<bitcoin::Amount>,
}

// --- /module/wallet/receive ---

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletReceiveRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WalletReceiveResponse {
    pub address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
}
