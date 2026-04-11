use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fmt::Debug;
use std::time::SystemTime;

use anyhow::Context as _;
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::sha256;
use bitcoin::secp256k1::PublicKey;
use bitcoin::{Address, Network, OutPoint};
use clap::Subcommand;
use envs::{
    FM_LDK_ALIAS_ENV, FM_LND_MACAROON_ENV, FM_LND_RPC_ADDR_ENV, FM_LND_TLS_CERT_ENV, FM_PORT_LDK,
};
use fedimint_connectors::error::ServerError;
use fedimint_connectors::{
    ConnectionPool, ConnectorRegistry, DynGatewayConnection, IGatewayConnection, ServerResult,
};
use fedimint_core::config::{FederationId, JsonClientConfig};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::hex::ToHex;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::util::SafeUrl;
use fedimint_core::{Amount, BitcoinAmountOrAll, secp256k1};
use fedimint_eventlog::{EventKind, EventLogId, PersistedLogEntry};
use futures::stream::BoxStream;
use lightning_invoice::{Bolt11Invoice, RoutingFees};
pub use reqwest::Method;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

pub mod envs;

pub const V1_API_ENDPOINT: &str = "v1";

pub const ADDRESS_ENDPOINT: &str = "/address";
pub const ADDRESS_RECHECK_ENDPOINT: &str = "/address_recheck";
pub const CONFIGURATION_ENDPOINT: &str = "/config";
pub const JOIN_ENDPOINT: &str = "/join";
pub const CREATE_BOLT11_INVOICE_FOR_OPERATOR_ENDPOINT: &str = "/create_bolt11_invoice_for_operator";
pub const GATEWAY_INFO_ENDPOINT: &str = "/info";
pub const INVITE_CODES_ENDPOINT: &str = "/invite_codes";
pub const GET_BALANCES_ENDPOINT: &str = "/balances";
pub const GET_INVOICE_ENDPOINT: &str = "/get_invoice";
pub const GET_LN_ONCHAIN_ADDRESS_ENDPOINT: &str = "/get_ln_onchain_address";
pub const LIST_CHANNELS_ENDPOINT: &str = "/list_channels";
pub const LIST_TRANSACTIONS_ENDPOINT: &str = "/list_transactions";
pub const MNEMONIC_ENDPOINT: &str = "/mnemonic";
pub const OPEN_CHANNEL_ENDPOINT: &str = "/open_channel";
pub const OPEN_CHANNEL_WITH_PUSH_ENDPOINT: &str = "/open_channel_with_push";
pub const CLOSE_CHANNELS_WITH_PEER_ENDPOINT: &str = "/close_channels_with_peer";
pub const PAY_INVOICE_FOR_OPERATOR_ENDPOINT: &str = "/pay_invoice_for_operator";
pub const PAYMENT_LOG_ENDPOINT: &str = "/payment_log";
pub const PEGIN_FROM_ONCHAIN_ENDPOINT: &str = "/pegin_from_onchain";
pub const RECEIVE_ECASH_ENDPOINT: &str = "/receive_ecash";
pub const SET_FEES_ENDPOINT: &str = "/set_fees";
pub const STOP_ENDPOINT: &str = "/stop";
pub const SEND_ONCHAIN_ENDPOINT: &str = "/send_onchain";
pub const SPEND_ECASH_ENDPOINT: &str = "/spend_ecash";
pub const WITHDRAW_ENDPOINT: &str = "/withdraw";
pub const WITHDRAW_TO_ONCHAIN_ENDPOINT: &str = "/withdraw_to_onchain";

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConnectFedPayload {
    pub invite_code: String,
    pub use_tor: Option<bool>,
    pub recover: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct InfoPayload;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConfigPayload {
    pub federation_id: Option<FederationId>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DepositAddressPayload {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PeginFromOnchainPayload {
    pub federation_id: FederationId,
    pub amount: BitcoinAmountOrAll,
    pub fee_rate_sats_per_vbyte: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DepositAddressRecheckPayload {
    pub address: Address<NetworkUnchecked>,
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WithdrawPayload {
    pub federation_id: FederationId,
    pub amount: BitcoinAmountOrAll,
    pub address: Address<NetworkUnchecked>,
    /// When provided (from UI preview flow), uses these quoted fees.
    /// When None, fetches current fees from the wallet.
    #[serde(default)]
    pub quoted_fees: Option<bitcoin::Amount>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WithdrawToOnchainPayload {
    pub federation_id: FederationId,
    pub amount: BitcoinAmountOrAll,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WithdrawResponse {
    pub txid: bitcoin::Txid,
    pub fees: bitcoin::Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WithdrawPreviewPayload {
    pub federation_id: FederationId,
    pub amount: BitcoinAmountOrAll,
    pub address: Address<NetworkUnchecked>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct WithdrawPreviewResponse {
    pub withdraw_amount: Amount,
    pub address: String,
    pub peg_out_fees: bitcoin::Amount,
    pub total_cost: Amount,
    /// Estimated mint fees when withdrawing all. None for partial withdrawals.
    #[serde(default)]
    pub mint_fees: Option<Amount>,
}

/// Deprecated, unused, doesn't do anything
///
/// Only here for backward-compat reasons.
#[allow(deprecated)]
#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub enum ConnectorType {
    Tcp,
    Tor,
}

#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable, Serialize, Deserialize)]
pub struct FederationConfig {
    pub invite_code: InviteCode,
    // Unique integer identifier per-federation that is assigned when the gateways joins a
    // federation.
    #[serde(alias = "mint_channel_id")]
    pub federation_index: u64,
    pub lightning_fee: PaymentFee,
    pub transaction_fee: PaymentFee,
    #[allow(deprecated)] // only here for decoding backward-compat
    pub _connector: ConnectorType,
}

/// Information about one of the feds we are connected to
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FederationInfo {
    pub federation_id: FederationId,
    pub federation_name: Option<String>,
    pub balance_msat: Amount,
    pub config: FederationConfig,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct GatewayInfo {
    pub version_hash: String,
    pub federations: Vec<FederationInfo>,
    /// Mapping from short channel id to the federation id that it belongs to.
    // TODO: Remove this alias once it no longer breaks backwards compatibility.
    #[serde(alias = "channels")]
    pub federation_fake_scids: Option<BTreeMap<u64, FederationId>>,
    pub gateway_state: String,
    pub lightning_info: LightningInfo,
    pub lightning_mode: LightningMode,
    pub registrations: BTreeMap<RegisteredProtocol, (SafeUrl, secp256k1::PublicKey)>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub struct GatewayFedConfig {
    pub federations: BTreeMap<FederationId, JsonClientConfig>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SetFeesPayload {
    pub federation_id: Option<FederationId>,
    pub lightning_base: Option<Amount>,
    pub lightning_parts_per_million: Option<u64>,
    pub transaction_base: Option<Amount>,
    pub transaction_parts_per_million: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateInvoiceForOperatorPayload {
    pub amount_msats: u64,
    pub expiry_secs: Option<u32>,
    pub description: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PayInvoiceForOperatorPayload {
    pub invoice: Bolt11Invoice,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpendEcashPayload {
    /// Federation id of the e-cash to spend
    pub federation_id: FederationId,
    /// The amount of e-cash to spend
    pub amount: Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpendEcashResponse {
    /// OOBNotes.to_string() for v1, base32::encode_prefixed() for v2
    pub notes: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReceiveEcashPayload {
    /// Can be OOBNotes (v1) or ECash (v2)
    pub notes: String,
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReceiveEcashResponse {
    pub amount: Amount,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct GatewayBalances {
    pub onchain_balance_sats: u64,
    pub lightning_balance_msats: u64,
    pub ecash_balances: Vec<FederationBalanceInfo>,
    pub inbound_lightning_liquidity_msats: u64,
}

#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct FederationBalanceInfo {
    pub federation_id: FederationId,
    pub ecash_balance_msats: Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct MnemonicResponse {
    pub mnemonic: Vec<String>,

    // Legacy federations are federations that the gateway joined prior to v0.5.0
    // and do not derive their secrets from the gateway's mnemonic. They also use
    // a separate database from the gateway's db.
    pub legacy_federations: Vec<FederationId>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PaymentLogPayload {
    // The position in the log to stop querying. No events will be returned from after
    // this `EventLogId`. If it is `None`, the last `EventLogId` is used.
    pub end_position: Option<EventLogId>,

    // The number of events to return
    pub pagination_size: usize,

    pub federation_id: FederationId,
    pub event_kinds: Vec<EventKind>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PaymentLogResponse(pub Vec<PersistedLogEntry>);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ChannelInfo {
    pub remote_pubkey: secp256k1::PublicKey,
    pub channel_size_sats: u64,
    pub outbound_liquidity_sats: u64,
    pub inbound_liquidity_sats: u64,
    pub is_active: bool,
    pub funding_outpoint: Option<OutPoint>,
    pub remote_node_alias: Option<String>,
    #[serde(default)]
    pub remote_address: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenChannelRequest {
    pub pubkey: secp256k1::PublicKey,
    pub host: String,
    pub channel_size_sats: u64,
    pub push_amount_sats: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SendOnchainRequest {
    pub address: Address<NetworkUnchecked>,
    pub amount: BitcoinAmountOrAll,
    pub fee_rate_sats_per_vbyte: u64,
}

impl fmt::Display for SendOnchainRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "SendOnchainRequest {{ address: {}, amount: {}, fee_rate_sats_per_vbyte: {} }}",
            self.address.assume_checked_ref(),
            self.amount,
            self.fee_rate_sats_per_vbyte
        )
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CloseChannelsWithPeerRequest {
    pub pubkey: secp256k1::PublicKey,
    #[serde(default)]
    pub force: bool,
    pub sats_per_vbyte: Option<u64>,
}

impl fmt::Display for CloseChannelsWithPeerRequest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "CloseChannelsWithPeerRequest {{ pubkey: {}, force: {}, sats_per_vbyte: {} }}",
            self.pubkey,
            self.force,
            match self.sats_per_vbyte {
                Some(sats) => sats.to_string(),
                None => "None".to_string(),
            }
        )
    }
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CloseChannelsWithPeerResponse {
    pub num_channels_closed: u32,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GetInvoiceRequest {
    pub payment_hash: sha256::Hash,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GetInvoiceResponse {
    pub preimage: Option<String>,
    pub payment_hash: Option<sha256::Hash>,
    pub amount: Amount,
    pub created_at: SystemTime,
    pub status: PaymentStatus,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListTransactionsPayload {
    pub start_secs: u64,
    pub end_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListTransactionsResponse {
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

#[derive(Debug, Clone, Subcommand, Serialize, Deserialize, Eq, PartialEq)]
pub enum LightningMode {
    #[clap(name = "lnd")]
    Lnd {
        /// LND RPC address
        #[arg(long = "lnd-rpc-host", env = FM_LND_RPC_ADDR_ENV)]
        lnd_rpc_addr: String,

        /// LND TLS cert file path
        #[arg(long = "lnd-tls-cert", env = FM_LND_TLS_CERT_ENV)]
        lnd_tls_cert: String,

        /// LND macaroon file path
        #[arg(long = "lnd-macaroon", env = FM_LND_MACAROON_ENV)]
        lnd_macaroon: String,
    },
    #[clap(name = "ldk")]
    Ldk {
        /// LDK lightning server port
        #[arg(long = "ldk-lightning-port", env = FM_PORT_LDK)]
        lightning_port: u16,

        /// LDK's Alias
        #[arg(long = "ldk-alias", env = FM_LDK_ALIAS_ENV)]
        alias: String,
    },
}

#[derive(Clone)]
pub enum ChainSource {
    Bitcoind {
        username: String,
        password: String,
        server_url: SafeUrl,
    },
    Esplora {
        server_url: SafeUrl,
    },
}

impl fmt::Display for ChainSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChainSource::Bitcoind {
                username: _,
                password: _,
                server_url,
            } => {
                write!(f, "Bitcoind source with URL: {server_url}")
            }
            ChainSource::Esplora { server_url } => {
                write!(f, "Esplora source with URL: {server_url}")
            }
        }
    }
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LightningInfo {
    Connected {
        public_key: PublicKey,
        alias: String,
        network: Network,
        block_height: u64,
        synced_to_chain: bool,
    },
    NotConnected,
}

#[derive(
    Debug, Clone, Eq, PartialEq, Ord, PartialOrd, Encodable, Decodable, Serialize, Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredProtocol {
    Http,
}

// --- PaymentFee moved from fedimint-lnv2-common ---

#[derive(
    Debug,
    Clone,
    Eq,
    PartialEq,
    PartialOrd,
    Hash,
    Serialize,
    Deserialize,
    Encodable,
    Decodable,
    Copy,
)]
pub struct PaymentFee {
    pub base: Amount,
    pub parts_per_million: u64,
}

impl PaymentFee {
    /// This is the maximum send fee of one and a half percent plus one hundred
    /// satoshis a correct gateway may recommend as a default. It accounts for
    /// the fee required to reliably route this payment over lightning.
    pub const SEND_FEE_LIMIT: PaymentFee = PaymentFee {
        base: Amount::from_sats(100),
        parts_per_million: 15_000,
    };

    /// This is the fee the gateway uses to cover transaction fees with the
    /// federation.
    pub const TRANSACTION_FEE_DEFAULT: PaymentFee = PaymentFee {
        base: Amount::from_sats(2),
        parts_per_million: 3000,
    };

    /// This is the maximum receive fee of half of one percent plus fifty
    /// satoshis a correct gateway may recommend as a default.
    pub const RECEIVE_FEE_LIMIT: PaymentFee = PaymentFee {
        base: Amount::from_sats(50),
        parts_per_million: 5_000,
    };

    pub fn add_to(&self, msats: u64) -> Amount {
        Amount::from_msats(msats.saturating_add(self.absolute_fee(msats)))
    }

    pub fn subtract_from(&self, msats: u64) -> Amount {
        Amount::from_msats(msats.saturating_sub(self.absolute_fee(msats)))
    }

    pub fn fee(&self, msats: u64) -> Amount {
        Amount::from_msats(self.absolute_fee(msats))
    }

    fn absolute_fee(&self, msats: u64) -> u64 {
        msats
            .saturating_mul(self.parts_per_million)
            .saturating_div(1_000_000)
            .checked_add(self.base.msats)
            .expect("The division creates sufficient headroom to add the base fee")
    }
}

impl std::ops::Add for PaymentFee {
    type Output = PaymentFee;
    fn add(self, rhs: Self) -> Self::Output {
        PaymentFee {
            base: self.base + rhs.base,
            parts_per_million: self.parts_per_million + rhs.parts_per_million,
        }
    }
}

impl From<RoutingFees> for PaymentFee {
    fn from(value: RoutingFees) -> Self {
        PaymentFee {
            base: Amount::from_msats(u64::from(value.base_msat)),
            parts_per_million: u64::from(value.proportional_millionths),
        }
    }
}

impl From<PaymentFee> for RoutingFees {
    fn from(value: PaymentFee) -> Self {
        RoutingFees {
            base_msat: u32::try_from(value.base.msats).expect("base msat was truncated from u64"),
            proportional_millionths: u32::try_from(value.parts_per_million)
                .expect("ppm was truncated from u64"),
        }
    }
}

impl std::fmt::Display for PaymentFee {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{},{}", self.base, self.parts_per_million)
    }
}

impl std::str::FromStr for PaymentFee {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = s.split(',');
        let base_str = parts
            .next()
            .ok_or(anyhow::anyhow!("Failed to parse base fee"))?;
        let ppm_str = parts.next().ok_or(anyhow::anyhow!("Failed to parse ppm"))?;

        // Ensure no extra parts
        if parts.next().is_some() {
            return Err(anyhow::anyhow!(
                "Failed to parse fees. Expected format <base>,<ppm>"
            ));
        }

        let base = Amount::from_str(base_str)?;
        let parts_per_million = ppm_str.parse::<u64>()?;

        Ok(PaymentFee {
            base,
            parts_per_million,
        })
    }
}

// --- Types moved from fedimint-lightning ---

pub const MAX_LIGHTNING_RETRIES: u32 = 10;

pub type RouteHtlcStream<'a> = BoxStream<'a, InterceptPaymentRequest>;

#[derive(
    Error, Debug, Serialize, Deserialize, Encodable, Decodable, Clone, Eq, PartialEq, Hash,
)]
pub enum LightningRpcError {
    #[error("Failed to connect to Lightning node")]
    FailedToConnect,
    #[error("Failed to retrieve node info: {failure_reason}")]
    FailedToGetNodeInfo { failure_reason: String },
    #[error("Failed to retrieve route hints: {failure_reason}")]
    FailedToGetRouteHints { failure_reason: String },
    #[error("Payment failed: {failure_reason}")]
    FailedPayment { failure_reason: String },
    #[error("Failed to route HTLCs: {failure_reason}")]
    FailedToRouteHtlcs { failure_reason: String },
    #[error("Failed to complete HTLC: {failure_reason}")]
    FailedToCompleteHtlc { failure_reason: String },
    #[error("Failed to open channel: {failure_reason}")]
    FailedToOpenChannel { failure_reason: String },
    #[error("Failed to close channel: {failure_reason}")]
    FailedToCloseChannelsWithPeer { failure_reason: String },
    #[error("Failed to get Invoice: {failure_reason}")]
    FailedToGetInvoice { failure_reason: String },
    #[error("Failed to list transactions: {failure_reason}")]
    FailedToListTransactions { failure_reason: String },
    #[error("Failed to get funding address: {failure_reason}")]
    FailedToGetLnOnchainAddress { failure_reason: String },
    #[error("Failed to withdraw funds on-chain: {failure_reason}")]
    FailedToWithdrawOnchain { failure_reason: String },
    #[error("Failed to connect to peer: {failure_reason}")]
    FailedToConnectToPeer { failure_reason: String },
    #[error("Failed to list active channels: {failure_reason}")]
    FailedToListChannels { failure_reason: String },
    #[error("Failed to get balances: {failure_reason}")]
    FailedToGetBalances { failure_reason: String },
    #[error("Failed to sync to chain: {failure_reason}")]
    FailedToSyncToChain { failure_reason: String },
    #[error("Invalid metadata: {failure_reason}")]
    InvalidMetadata { failure_reason: String },
    #[error("Bolt12 Error: {failure_reason}")]
    Bolt12Error { failure_reason: String },
}

/// Simplified lightning context without trait object.
#[derive(Clone, Debug)]
pub struct LightningContext {
    pub lightning_public_key: PublicKey,
    pub lightning_alias: String,
    pub lightning_network: Network,
    pub supports_private_payments: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GetNodeInfoResponse {
    pub pub_key: PublicKey,
    pub alias: String,
    pub network: String,
    pub block_height: u32,
    pub synced_to_chain: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InterceptPaymentRequest {
    pub payment_hash: sha256::Hash,
    pub amount_msat: u64,
    pub expiry: u32,
    pub incoming_chan_id: u64,
    pub short_channel_id: Option<u64>,
    pub htlc_id: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct InterceptPaymentResponse {
    pub incoming_chan_id: u64,
    pub htlc_id: u64,
    pub payment_hash: sha256::Hash,
    pub action: PaymentAction,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum PaymentAction {
    Settle(Preimage),
    Cancel,
    Forward,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GetRouteHintsResponse {
    pub route_hints: Vec<RouteHint>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PayInvoiceResponse {
    pub preimage: Preimage,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateInvoiceRequest {
    pub payment_hash: Option<sha256::Hash>,
    pub amount_msat: u64,
    pub expiry_secs: u32,
    pub description: Option<InvoiceDescription>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub enum InvoiceDescription {
    Direct(String),
    Hash(sha256::Hash),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct CreateInvoiceResponse {
    pub invoice: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GetLnOnchainAddressResponse {
    pub address: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SendOnchainResponse {
    pub txid: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct OpenChannelResponse {
    pub funding_txid: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListChannelsResponse {
    pub channels: Vec<ChannelInfo>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct GetBalancesResponse {
    pub onchain_balance_sats: u64,
    pub lightning_balance_msats: u64,
    pub inbound_lightning_liquidity_msats: u64,
}

// --- Types moved from fedimint-ln-common ---

#[derive(Debug, Clone, Eq, PartialEq, Hash, Deserialize, Serialize, Encodable, Decodable)]
pub struct Preimage(pub [u8; 32]);

impl std::fmt::Display for Preimage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0.encode_hex::<String>())
    }
}

// TODO: upstream serde support to LDK
/// Hack to get a route hint that implements `serde` traits.
pub mod route_hints {
    use fedimint_core::encoding::{Decodable, Encodable};
    use fedimint_core::secp256k1::PublicKey;
    use lightning_invoice::RoutingFees;
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
    pub struct RouteHintHop {
        /// The `node_id` of the non-target end of the route
        pub src_node_id: PublicKey,
        /// The `short_channel_id` of this channel
        pub short_channel_id: u64,
        /// Flat routing fee in millisatoshis
        pub base_msat: u32,
        /// Liquidity-based routing fee in millionths of a routed amount.
        /// In other words, 10000 is 1%.
        pub proportional_millionths: u32,
        /// The difference in CLTV values between this node and the next node.
        pub cltv_expiry_delta: u16,
        /// The minimum value, in msat, which must be relayed to the next hop.
        pub htlc_minimum_msat: Option<u64>,
        /// The maximum value in msat available for routing with a single HTLC.
        pub htlc_maximum_msat: Option<u64>,
    }

    /// A list of hops along a payment path terminating with a channel to the
    /// recipient.
    #[derive(Clone, Debug, Hash, Eq, PartialEq, Serialize, Deserialize, Encodable, Decodable)]
    pub struct RouteHint(pub Vec<RouteHintHop>);

    impl RouteHint {
        pub fn to_ldk_route_hint(&self) -> lightning_invoice::RouteHint {
            lightning_invoice::RouteHint(
                self.0
                    .iter()
                    .map(|hop| lightning_invoice::RouteHintHop {
                        src_node_id: hop.src_node_id,
                        short_channel_id: hop.short_channel_id,
                        fees: RoutingFees {
                            base_msat: hop.base_msat,
                            proportional_millionths: hop.proportional_millionths,
                        },
                        cltv_expiry_delta: hop.cltv_expiry_delta,
                        htlc_minimum_msat: hop.htlc_minimum_msat,
                        htlc_maximum_msat: hop.htlc_maximum_msat,
                    })
                    .collect(),
            )
        }
    }

    impl From<lightning_invoice::RouteHint> for RouteHint {
        fn from(rh: lightning_invoice::RouteHint) -> Self {
            RouteHint(rh.0.into_iter().map(Into::into).collect())
        }
    }

    impl From<lightning_invoice::RouteHintHop> for RouteHintHop {
        fn from(rhh: lightning_invoice::RouteHintHop) -> Self {
            RouteHintHop {
                src_node_id: rhh.src_node_id,
                short_channel_id: rhh.short_channel_id,
                base_msat: rhh.fees.base_msat,
                proportional_millionths: rhh.fees.proportional_millionths,
                cltv_expiry_delta: rhh.cltv_expiry_delta,
                htlc_minimum_msat: rhh.htlc_minimum_msat,
                htlc_maximum_msat: rhh.htlc_maximum_msat,
            }
        }
    }
}

pub use route_hints::RouteHint;

// TODO: Upstream serde serialization for
// lightning_invoice::RoutingFees
// See https://github.com/lightningdevkit/rust-lightning/blob/b8ed4d2608e32128dd5a1dee92911638a4301138/lightning/src/routing/gossip.rs#L1057-L1065
pub mod serde_routing_fees {
    use lightning_invoice::RoutingFees;
    use serde::ser::SerializeStruct;
    use serde::{Deserialize, Deserializer, Serializer};

    #[allow(missing_docs)]
    pub fn serialize<S>(fees: &RoutingFees, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("RoutingFees", 2)?;
        state.serialize_field("base_msat", &fees.base_msat)?;
        state.serialize_field("proportional_millionths", &fees.proportional_millionths)?;
        state.end()
    }

    #[allow(missing_docs)]
    pub fn deserialize<'de, D>(deserializer: D) -> Result<RoutingFees, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fees = serde_json::Value::deserialize(deserializer)?;
        // While we deserialize fields as u64, RoutingFees expects u32 for the fields
        let base_msat = fees["base_msat"]
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("base_msat is not a u64"))?;
        let proportional_millionths = fees["proportional_millionths"]
            .as_u64()
            .ok_or_else(|| serde::de::Error::custom("proportional_millionths is not a u64"))?;

        Ok(RoutingFees {
            base_msat: base_msat
                .try_into()
                .map_err(|_| serde::de::Error::custom("base_msat is greater than u32::MAX"))?,
            proportional_millionths: proportional_millionths.try_into().map_err(|_| {
                serde::de::Error::custom("proportional_millionths is greater than u32::MAX")
            })?,
        })
    }
}

/// Data needed to pay an invoice
///
/// This is a subset of the data from a [`lightning_invoice::Bolt11Invoice`]
/// that does not contain the description, which increases privacy for the user.
#[derive(Debug, Clone, Eq, PartialEq, Hash, Serialize, Deserialize, Decodable, Encodable)]
pub struct PrunedInvoice {
    pub amount: Amount,
    pub destination: secp256k1::PublicKey,
    /// Wire-format encoding of feature bit vector
    #[serde(with = "fedimint_core::hex::serde", default)]
    pub destination_features: Vec<u8>,
    pub payment_hash: sha256::Hash,
    pub payment_secret: [u8; 32],
    pub route_hints: Vec<RouteHint>,
    pub min_final_cltv_delta: u64,
    /// Time at which the invoice expires in seconds since unix epoch
    pub expiry_timestamp: u64,
}

impl PrunedInvoice {
    pub fn new(invoice: &Bolt11Invoice, amount: Amount) -> Self {
        // We use expires_at since it doesn't rely on the std feature in
        // lightning-invoice. See #3838.
        let expiry_timestamp = invoice.expires_at().map_or(u64::MAX, |t| t.as_secs());

        let destination_features = if let Some(features) = invoice.features() {
            fedimint_core::encode_bolt11_invoice_features_without_length(features)
        } else {
            vec![]
        };

        PrunedInvoice {
            amount,
            destination: invoice
                .payee_pub_key()
                .copied()
                .unwrap_or_else(|| invoice.recover_payee_pub_key()),
            destination_features,
            payment_hash: *invoice.payment_hash(),
            payment_secret: invoice.payment_secret().0,
            route_hints: invoice.route_hints().into_iter().map(Into::into).collect(),
            min_final_cltv_delta: invoice.min_final_cltv_expiry_delta(),
            expiry_timestamp,
        }
    }
}

impl TryFrom<Bolt11Invoice> for PrunedInvoice {
    type Error = anyhow::Error;

    fn try_from(invoice: Bolt11Invoice) -> Result<Self, Self::Error> {
        Ok(PrunedInvoice::new(
            &invoice,
            Amount::from_msats(
                invoice
                    .amount_milli_satoshis()
                    .context("Invoice amount is missing")?,
            ),
        ))
    }
}

// --- Types moved from fedimint-ln-common/src/client.rs ---

#[derive(Clone, Debug)]
pub struct GatewayApi {
    password: Option<String>,
    connection_pool: ConnectionPool<dyn IGatewayConnection>,
}

impl GatewayApi {
    pub fn new(password: Option<String>, connectors: ConnectorRegistry) -> Self {
        Self {
            password,
            connection_pool: ConnectionPool::new(connectors),
        }
    }

    async fn get_or_create_connection(&self, url: &SafeUrl) -> ServerResult<DynGatewayConnection> {
        self.connection_pool
            .get_or_create_connection(url, None, |url, _api_secret, connectors| async move {
                let conn = connectors
                    .connect_gateway(&url)
                    .await
                    .map_err(ServerError::Connection)?;
                Ok(conn)
            })
            .await
    }

    pub async fn request<P: Serialize, T: DeserializeOwned>(
        &self,
        base_url: &SafeUrl,
        method: Method,
        route: &str,
        payload: Option<P>,
    ) -> ServerResult<T> {
        let conn = self
            .get_or_create_connection(base_url)
            .await
            .context("Failed to connect to gateway")
            .map_err(ServerError::Connection)?;
        let payload = payload.map(|p| serde_json::to_value(p).expect("Could not serialize"));
        let res = conn
            .request(self.password.clone(), method, route, payload)
            .await?;
        let response = serde_json::from_value::<T>(res).map_err(|e| {
            ServerError::InvalidResponse(anyhow::anyhow!("Received invalid response: {e}"))
        })?;
        Ok(response)
    }

    /// Get receiver for changes in the active connections
    ///
    /// This allows real-time monitoring of connection status.
    pub fn get_active_connection_receiver(&self) -> watch::Receiver<BTreeSet<SafeUrl>> {
        self.connection_pool.get_active_connection_receiver()
    }
}
