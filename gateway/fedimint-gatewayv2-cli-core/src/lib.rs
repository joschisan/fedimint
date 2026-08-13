//! Route + payload contract shared by the `gatewaydv2` daemon and its admin
//! CLI (`gatewaydv2-cli`).
//!
//! The daemon serves these routes over a Unix socket at
//! `{DATA_DIR}/{CLI_SOCKET_FILENAME}`; the CLI POSTs JSON request bodies and
//! pretty-prints the JSON responses. Request types derive [`clap::Args`] so the
//! CLI's command tree and the daemon's handlers stay in sync at compile time.
//!
//! Modelled on picomint's gateway CLI, adapted to fedimint types.
//! Per-federation commands take the federation id as a required positional
//! argument.

use std::collections::BTreeMap;

use bitcoin::address::NetworkUnchecked;
use bitcoin::secp256k1::PublicKey;
use clap::Args;
use fedimint_core::Amount;
use fedimint_core::config::FederationId;
use fedimint_core::invite_code::InviteCode;
use fedimint_mintv2_common::Denomination;
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};

/// Filename of the gateway's admin CLI Unix socket, inside `DATA_DIR`. The
/// daemon binds and the CLI connects at `{DATA_DIR}/{CLI_SOCKET_FILENAME}`.
pub const CLI_SOCKET_FILENAME: &str = "cli.sock";

// Top-level
pub const ROUTE_INFO: &str = "/info";
pub const ROUTE_MNEMONIC: &str = "/mnemonic";

// LDK node management
pub const ROUTE_LDK_BALANCES: &str = "/ldk/balances";
pub const ROUTE_LDK_ONCHAIN_RECEIVE: &str = "/ldk/onchain/receive";
pub const ROUTE_LDK_ONCHAIN_SEND: &str = "/ldk/onchain/send";
pub const ROUTE_LDK_CHANNEL_OPEN: &str = "/ldk/channel/open";
pub const ROUTE_LDK_CHANNEL_CLOSE: &str = "/ldk/channel/close";
pub const ROUTE_LDK_CHANNEL_LIST: &str = "/ldk/channel/list";
pub const ROUTE_LDK_CHANNEL_SPLICE_IN: &str = "/ldk/channel/splice-in";
pub const ROUTE_LDK_CHANNEL_SPLICE_OUT: &str = "/ldk/channel/splice-out";
pub const ROUTE_LDK_LN_RECEIVE: &str = "/ldk/ln/receive";
pub const ROUTE_LDK_LN_SEND: &str = "/ldk/ln/send";
pub const ROUTE_LDK_LN_PROBE: &str = "/ldk/ln/probe";
pub const ROUTE_LDK_PEER_CONNECT: &str = "/ldk/peer/connect";
pub const ROUTE_LDK_PEER_DISCONNECT: &str = "/ldk/peer/disconnect";
pub const ROUTE_LDK_PEER_LIST: &str = "/ldk/peer/list";

// Federation management
pub const ROUTE_FEDERATION_JOIN: &str = "/federation/join";
pub const ROUTE_FEDERATION_DISABLE: &str = "/federation/disable";
pub const ROUTE_FEDERATION_ENABLE: &str = "/federation/enable";
pub const ROUTE_FEDERATION_LIST: &str = "/federation/list";
pub const ROUTE_FEDERATION_CONFIG: &str = "/federation/config";
pub const ROUTE_FEDERATION_BALANCE: &str = "/federation/balance";

// Per-federation module commands
pub const ROUTE_FEDERATION_MODULE_MINT_COUNT: &str = "/federation/module/mint/count";
pub const ROUTE_FEDERATION_MODULE_MINT_SEND: &str = "/federation/module/mint/send";
pub const ROUTE_FEDERATION_MODULE_MINT_RECEIVE: &str = "/federation/module/mint/receive";
pub const ROUTE_FEDERATION_MODULE_WALLET_SEND_FEE: &str = "/federation/module/wallet/send-fee";
pub const ROUTE_FEDERATION_MODULE_WALLET_SEND: &str = "/federation/module/wallet/send";
pub const ROUTE_FEDERATION_MODULE_WALLET_RECEIVE: &str = "/federation/module/wallet/receive";

// --- /info ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InfoResponse {
    /// Lightning node public key (LDK node id).
    pub lightning_pk: PublicKey,
    pub network: String,
    pub block_height: u64,
    pub synced_to_chain: bool,
}

// --- /mnemonic ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MnemonicResponse {
    pub mnemonic: Vec<String>,
}

// --- /ldk/balances ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkBalancesResponse {
    /// Everything the on-chain wallet holds, including unconfirmed funds and
    /// the anchor reserve below.
    pub total_onchain_balance_sat: u64,
    /// The share of the on-chain wallet we can spend right now: sufficiently
    /// confirmed, minus the anchor reserve below.
    pub spendable_onchain_balance_sat: u64,
    /// On-chain funds withheld so we can always bump a channel's anchor output
    /// to get its force-close transaction confirmed.
    pub total_anchor_channels_reserve_sat: u64,
    /// What our usable channels can still receive.
    pub total_inbound_capacity_sat: u64,
    /// What our usable channels can still send.
    pub total_outbound_capacity_sat: u64,
    /// The largest single payment each usable channel will still forward,
    /// summed. Sits below the outbound capacity, which one payment cannot
    /// exhaust.
    pub total_next_outbound_htlc_limit_sat: u64,
    /// What we could claim across all channels, including the timelocked side
    /// of a channel that has already been closed.
    pub total_lightning_balance_sat: u64,
    /// Funds from closed channels that are on their way back to the on-chain
    /// wallet but are not swept into it yet.
    pub total_pending_closure_balance_sat: u64,
}

// --- /ldk/onchain/receive ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkOnchainReceiveResponse {
    pub address: bitcoin::Address<NetworkUnchecked>,
}

// --- /ldk/onchain/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkOnchainSendRequest {
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub amount: bitcoin::Amount,
    #[arg(long)]
    pub sat_per_vbyte: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkOnchainSendResponse {
    pub txid: bitcoin::Txid,
}

// --- /ldk/channel/open ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelOpenRequest {
    pub pubkey: PublicKey,
    pub host: String,
    pub channel_size_sat: u64,
    #[arg(long, default_value_t = 0)]
    pub push_amount_sat: u64,
    /// Announce the channel to the network so other nodes can route through
    /// it. Requires the node to be configured with a listening address and an
    /// alias.
    #[arg(long)]
    #[serde(default)]
    pub announce: bool,
}

// --- /ldk/channel/close ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelCloseRequest {
    pub pubkey: PublicKey,
    #[arg(long)]
    #[serde(default)]
    pub force: bool,
    #[arg(long, required_unless_present = "force")]
    pub sat_per_vbyte: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkChannelCloseResponse {
    pub num_channels_closed: u32,
}

// --- /ldk/channel/list ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkChannelListResponse {
    pub channels: Vec<ChannelInfo>,
}

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelInfo {
    /// Local identifier of the channel, as accepted by the splice commands.
    #[serde_as(as = "DisplayFromStr")]
    pub user_channel_id: u128,
    pub remote_pubkey: PublicKey,
    pub remote_alias: Option<String>,
    pub remote_address: Option<String>,
    pub channel_size_sat: u64,
    pub outbound_liquidity_sat: u64,
    pub next_outbound_htlc_limit_sat: u64,
    pub inbound_liquidity_sat: u64,
    pub is_usable: bool,
    pub is_outbound: bool,
    /// Whether the channel is, or once confirmed will be, publicly announced.
    pub is_announced: bool,
    pub funding_txid: Option<bitcoin::Txid>,
}

// --- /ldk/channel/splice-in ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelSpliceInRequest {
    /// Channel to splice on-chain funds into, as reported by `channel list`.
    /// Identifies the channel rather than the peer, since a peer may hold
    /// several.
    #[serde_as(as = "DisplayFromStr")]
    pub user_channel_id: u128,
    /// Peer the channel is with.
    pub pubkey: PublicKey,
    /// On-chain funds to add to the channel, in satoshi.
    pub amount_sat: u64,
}

// --- /ldk/channel/splice-out ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelSpliceOutRequest {
    /// Channel to splice funds out of, as reported by `channel list`.
    /// Identifies the channel rather than the peer, since a peer may hold
    /// several.
    #[serde_as(as = "DisplayFromStr")]
    pub user_channel_id: u128,
    /// Peer the channel is with.
    pub pubkey: PublicKey,
    /// Destination on-chain address for the spliced-out funds.
    pub address: bitcoin::Address<NetworkUnchecked>,
    /// Amount to remove from the channel, in satoshi (must not exceed the
    /// channel's outbound capacity).
    pub amount_sat: u64,
}

// --- /ldk/ln/receive ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkLnReceiveRequest {
    pub amount_msat: u64,
    #[arg(long)]
    pub expiry_secs: Option<u32>,
    #[arg(long)]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkLnReceiveResponse {
    pub invoice: String,
}

// --- /ldk/ln/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkLnSendRequest {
    pub invoice: Bolt11Invoice,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkLnSendResponse {
    pub preimage: String,
}

// --- /ldk/ln/probe ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkLnProbeRequest {
    /// The node to probe a route towards.
    pub node_id: PublicKey,
    /// The amount to find paths for, in millisatoshis.
    pub amount_msat: u64,
}

// --- /ldk/peer/connect ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkPeerConnectRequest {
    pub pubkey: PublicKey,
    pub host: String,
}

// --- /ldk/peer/disconnect ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkPeerDisconnectRequest {
    pub pubkey: PublicKey,
}

// --- /ldk/peer/list ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LdkPeerListResponse {
    pub peers: Vec<PeerInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: PublicKey,
    pub address: String,
    pub is_connected: bool,
}

// --- /federation/join ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationJoinRequest {
    pub invite: InviteCode,
}

// --- /federation/disable + /federation/enable ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationDisableRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationEnableRequest {
    pub federation_id: FederationId,
}

// --- /federation/list ---

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationListResponse {
    pub federations: Vec<FederationInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationInfo {
    pub federation_id: FederationId,
    pub federation_name: Option<String>,
}

// --- /federation/config ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationConfigRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationConfigResponse {
    pub config: serde_json::Value,
}

// --- /federation/balance ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationBalanceRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationBalanceResponse {
    pub balance_msat: Amount,
}

// --- /federation/module/mint/count ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationMintCountRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationMintCountResponse {
    /// Count of held ecash notes keyed by denomination.
    pub counts: BTreeMap<Denomination, u64>,
}

// --- /federation/module/mint/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationMintSendRequest {
    pub federation_id: FederationId,
    pub amount: Amount,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationMintSendResponse {
    /// The serialized ecash notes bundle.
    pub ecash: String,
}

// --- /federation/module/mint/receive ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationMintReceiveRequest {
    pub federation_id: FederationId,
    /// The serialized ecash notes bundle.
    pub ecash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationMintReceiveResponse {
    pub amount: Amount,
}

// --- /federation/module/wallet/send-fee ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationWalletSendFeeRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationWalletSendFeeResponse {
    pub fee: bitcoin::Amount,
}

// --- /federation/module/wallet/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationWalletSendRequest {
    pub federation_id: FederationId,
    pub address: bitcoin::Address<NetworkUnchecked>,
    pub amount: bitcoin::Amount,
    #[arg(long)]
    pub fee: Option<bitcoin::Amount>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationWalletSendResponse {
    pub txid: bitcoin::Txid,
}

// --- /federation/module/wallet/receive ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationWalletReceiveRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FederationWalletReceiveResponse {
    pub address: bitcoin::Address<NetworkUnchecked>,
}
