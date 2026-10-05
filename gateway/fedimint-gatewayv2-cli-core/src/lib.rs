//! Route + payload contract shared by the `gatewaydv2` daemon and its admin
//! CLI (`gatewaydv2-cli`).
//!
//! The daemon serves these routes over a Unix socket at
//! `{DATA_DIR}/{CLI_SOCKET_FILENAME}`; the CLI POSTs JSON request bodies and
//! pretty-prints the JSON responses. Request types derive [`clap::Args`] so the
//! CLI's command tree and the daemon's handlers stay in sync at compile time.
//! Every response type carries a JSON Schema, printed at the end of its
//! command's `--help`, so its doc lines are the operator-facing description
//! of each field. Every error a handler refuses a request with is one of the
//! `thiserror` enums at the bottom, deriving [`ErrorCode`]: the variant is
//! the code the CLI prints and `--help` lists.
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
use fedimint_core::error::ErrorCode;
use fedimint_core::invite_code::InviteCode;
use fedimint_mintv2_client::{ReceiveECashError, SendECashError};
use fedimint_mintv2_common::Denomination;
use lightning::ln::msgs::SocketAddress;
use lightning_invoice::Bolt11Invoice;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_with::{DisplayFromStr, serde_as};
use thiserror::Error;

/// Filename of the gateway's admin CLI Unix socket, inside `DATA_DIR`. The
/// daemon binds and the CLI connects at `{DATA_DIR}/{CLI_SOCKET_FILENAME}`.
pub const CLI_SOCKET_FILENAME: &str = "cli.sock";

// Top-level
pub const ROUTE_INFO: &str = "/info";
pub const ROUTE_MNEMONIC: &str = "/mnemonic";
pub const ROUTE_QUERY: &str = "/query";

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

/// The SQL that creates the analytics database the `query` command reads:
/// one table per gwv2 payment event, tagged with the federation it came
/// from, and the `outgoing_payments` / `incoming_payments` views that join
/// the event tables into one row per payment. The daemon installs this on
/// every start, so `query --help` prints the schema with no daemon running.
pub const ANALYTICS_SCHEMA_SQL: &str = r"
-- An outgoing payment's start: the sender's contract is confirmed and the
-- gateway is about to pay. `ln_fee_msat` is the routing-fee budget; the
-- gateway keeps whatever routing does not take.
CREATE TABLE send (
    federation       TEXT NOT NULL,
    payment_image    TEXT NOT NULL,
    ts               INTEGER NOT NULL,   -- msecs since unix epoch
    amount_msat      INTEGER NOT NULL,
    ln_fee_msat      INTEGER NOT NULL,
    fee_msat         INTEGER NOT NULL,
    destination_node TEXT NOT NULL,      -- pubkey of the node we pay
    PRIMARY KEY (federation, payment_image)
);

-- The gateway claimed the sender's contract with the preimage;
-- `ln_fee_msat` is the routing fee the payment actually cost.
CREATE TABLE send_success (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    preimage      TEXT NOT NULL,
    ln_fee_msat   INTEGER NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

-- The gateway forfeited the sender's contract; `error` says why.
CREATE TABLE send_cancel (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    error         TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

-- An incoming payment's start: the gateway funded the recipient's contract.
CREATE TABLE receive (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    amount_msat   INTEGER NOT NULL,
    fee_msat      INTEGER NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

-- The federation accepted the funding and revealed the preimage.
CREATE TABLE receive_success (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    preimage      TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

-- The federation rejected the funding; `error` says why.
CREATE TABLE receive_failure (
    federation    TEXT NOT NULL,
    payment_image TEXT NOT NULL,
    ts            INTEGER NOT NULL,
    error         TEXT NOT NULL,
    PRIMARY KEY (federation, payment_image)
);

CREATE INDEX idx_send_ts            ON send(ts);
CREATE INDEX idx_send_success_ts    ON send_success(ts);
CREATE INDEX idx_receive_ts         ON receive(ts);
CREATE INDEX idx_receive_success_ts ON receive_success(ts);

-- One row per outgoing payment. `direct` marks a swap between two
-- federations of this gateway, which has a receive row with the same
-- payment image and costs no routing fee.
CREATE VIEW outgoing_payments AS
SELECT
    s.federation,
    s.payment_image,
    s.ts AS started_at,
    COALESCE(succ.ts, canc.ts) AS completed_at,
    CASE
        WHEN succ.payment_image IS NOT NULL THEN 'success'
        WHEN canc.payment_image IS NOT NULL THEN 'cancelled'
        ELSE 'pending'
    END AS status,
    EXISTS(SELECT 1 FROM receive r WHERE r.payment_image = s.payment_image) AS direct,
    s.destination_node,
    s.amount_msat,
    s.fee_msat       AS gw_fee_msat,
    s.ln_fee_msat    AS ln_fee_budget_msat,
    CASE
        WHEN succ.payment_image IS NOT NULL THEN succ.ln_fee_msat
        WHEN canc.payment_image IS NOT NULL THEN 0
        ELSE NULL
    END AS ln_fee_paid_msat,
    CASE
        WHEN succ.payment_image IS NOT NULL THEN s.ln_fee_msat - succ.ln_fee_msat
        WHEN canc.payment_image IS NOT NULL THEN 0
        ELSE NULL
    END AS ln_fee_kept_msat,
    succ.preimage,
    canc.error
FROM send s
LEFT JOIN send_success succ
       ON succ.federation = s.federation AND succ.payment_image = s.payment_image
LEFT JOIN send_cancel canc
       ON canc.federation = s.federation AND canc.payment_image = s.payment_image;

-- One row per incoming payment, `direct` as above.
CREATE VIEW incoming_payments AS
SELECT
    r.federation,
    r.payment_image,
    r.ts AS started_at,
    COALESCE(succ.ts, fail.ts) AS completed_at,
    CASE
        WHEN succ.payment_image IS NOT NULL THEN 'success'
        WHEN fail.payment_image IS NOT NULL THEN 'failure'
        ELSE 'pending'
    END AS status,
    EXISTS(SELECT 1 FROM send s WHERE s.payment_image = r.payment_image) AS direct,
    r.amount_msat,
    r.fee_msat       AS gw_fee_msat,
    succ.preimage,
    fail.error
FROM receive r
LEFT JOIN receive_success succ
       ON succ.federation = r.federation AND succ.payment_image = r.payment_image
LEFT JOIN receive_failure fail
       ON fail.federation = r.federation AND fail.payment_image = r.payment_image;
";

// --- /info ---

/// The state of the gateway's Lightning node. The gateway is one LDK node
/// facing the Lightning Network and one client of every federation it
/// serves; `lightning_pk` names the former.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct InfoResponse {
    /// The LDK node's id, a hex secp256k1 public key: what Lightning peers
    /// and LSPs connect to and open channels with
    #[schemars(with = "String")]
    pub lightning_pk: PublicKey,
    /// The Bitcoin network the LDK node runs on, from `FM_NETWORK`; every
    /// federation the gateway serves has to run on it
    pub network: String,
    /// The LDK node's best block height as its chain source, bitcoind or
    /// esplora, reports it; each federation keeps its own consensus height
    pub block_height: u64,
    /// Whether the LDK wallet has completed a chain sync since the daemon
    /// started; balances and channel states are stale until it has
    pub synced_to_chain: bool,
}

// --- /mnemonic ---

/// The seed. Secret: pipe it into a file, never to a terminal.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MnemonicResponse {
    /// The twelve BIP39 words the LDK onchain wallet and every federation
    /// balance derive from; channel balances do not
    pub mnemonic: Vec<String>,
}

// --- /query ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct QueryRequest {
    /// Read-only SQL run against the analytics db, e.g.
    /// "SELECT * FROM outgoing_payments ORDER BY started_at DESC LIMIT 10"
    pub query: String,
}

/// The rows the query returned: one JSON object per row, keyed by result
/// column name, the same shape `sqlite3 --json` prints. Column types follow
/// the analytics schema: timestamps as msecs since the unix epoch, amounts
/// as msat integers, payment images, preimages and node ids as hex text.
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct QueryResponse(pub Vec<serde_json::Map<String, serde_json::Value>>);

// --- /ldk/balances ---

/// Everything the LDK node holds, onchain and in channels, in sat. Ecash
/// held in federations is not part of this; `federation balance` reports
/// it per federation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkBalancesResponse {
    /// Everything the on-chain wallet holds, including unconfirmed funds and
    /// the anchor reserve below
    pub total_onchain_balance_sat: u64,
    /// The share of the on-chain wallet we can spend right now: sufficiently
    /// confirmed, minus the anchor reserve below
    pub spendable_onchain_balance_sat: u64,
    /// What we could claim across all channels, including the timelocked side
    /// of a channel that has already been closed
    pub total_lightning_balance_sat: u64,
    /// On-chain funds withheld so we can always bump a channel's anchor output
    /// to get its force-close transaction confirmed
    pub total_anchor_channels_reserve_sat: u64,
    /// What our usable channels can still receive
    pub total_inbound_capacity_sat: u64,
    /// What our usable channels can still send
    pub total_outbound_capacity_sat: u64,
    /// The largest single payment each usable channel will still forward,
    /// summed. Sits below the outbound capacity, which one payment cannot
    /// exhaust
    pub total_next_outbound_htlc_limit_sat: u64,
}

// --- /ldk/onchain/receive ---

/// An address of the LDK onchain wallet.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkOnchainReceiveResponse {
    /// A fresh receive address of the LDK onchain wallet; funds sent here
    /// are what `channel open` spends
    #[schemars(with = "String")]
    pub address: bitcoin::Address<NetworkUnchecked>,
}

// --- /ldk/onchain/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkOnchainSendRequest {
    /// The destination address, on the LDK node's network
    pub address: bitcoin::Address<NetworkUnchecked>,
    /// The amount with its denomination, e.g. "100000 sat" or "0.001 BTC"
    pub amount: bitcoin::Amount,
    /// The fee rate to pay, in sat/vB
    #[arg(long)]
    pub sat_per_vbyte: u64,
}

/// The broadcast transaction.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkOnchainSendResponse {
    /// Id of the transaction the LDK wallet broadcast, hex
    #[schemars(with = "String")]
    pub txid: bitcoin::Txid,
}

// --- /ldk/channel/open ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelOpenRequest {
    /// The peer's node id, hex
    pub pubkey: PublicKey,
    /// The peer's `host:port`: an IP, a hostname or an onion address
    #[serde_as(as = "DisplayFromStr")]
    pub host: SocketAddress,
    /// The channel's capacity in sat, funded from the LDK onchain wallet
    pub channel_size_sat: u64,
    /// Sat handed to the peer as its starting balance in the channel
    #[arg(long, default_value_t = 0)]
    pub push_amount_sat: u64,
    /// Announce the channel to the network so other nodes can route through
    /// it. Requires the node to be configured with a listening address and an
    /// alias
    #[arg(long)]
    #[serde(default)]
    pub announce: bool,
}

// --- /ldk/channel/close ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelCloseRequest {
    /// Channel to close, as reported by `channel list`. Identifies the channel
    /// rather than the peer, since a peer may hold several
    #[serde_as(as = "DisplayFromStr")]
    pub user_channel_id: u128,
    /// Peer the channel is with
    pub pubkey: PublicKey,
    /// Close unilaterally instead of negotiating with the peer; our balance
    /// then comes back after the channel's timelock
    #[arg(long)]
    #[serde(default)]
    pub force: bool,
}

// --- /ldk/channel/list ---

/// Every channel the LDK node has, open or still confirming.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkChannelListResponse {
    /// The channels, in no particular order
    pub channels: Vec<ChannelInfo>,
}

/// One channel with its liquidity in both directions, in sat.
#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ChannelInfo {
    /// Local identifier of the channel, as accepted by the close and splice
    /// commands; a decimal string
    #[serde_as(as = "DisplayFromStr")]
    #[schemars(with = "String")]
    pub user_channel_id: u128,
    /// The peer's node id, hex
    #[schemars(with = "String")]
    pub remote_pubkey: PublicKey,
    /// The peer's alias from its node announcement; absent if it announces
    /// none or the network graph does not know the peer
    pub remote_alias: Option<String>,
    /// The peer's socket address as the LDK node has it on record; absent
    /// if the peer is not in `peer list`
    pub remote_address: Option<String>,
    /// The channel's capacity in sat
    pub channel_size_sat: u64,
    /// What this node can send over the channel right now, in sat: its
    /// balance less the reserve and in-flight payments
    pub outbound_liquidity_sat: u64,
    /// The largest single payment the channel will forward right now, in
    /// sat; at or below `outbound_liquidity_sat`
    pub next_outbound_htlc_limit_sat: u64,
    /// What the peer can send this way right now, in sat
    pub inbound_liquidity_sat: u64,
    /// Whether the channel can carry payments right now: funding confirmed
    /// and the peer connected
    pub is_usable: bool,
    /// Whether this node opened and funded the channel
    pub is_outbound: bool,
    /// Whether the channel is, or once confirmed will be, publicly announced
    pub is_announced: bool,
    /// Id of the funding transaction, hex; absent only while it has not been
    /// created yet
    #[schemars(with = "Option<String>")]
    pub funding_txid: Option<bitcoin::Txid>,
}

// --- /ldk/channel/splice-in ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelSpliceInRequest {
    /// Channel to splice on-chain funds into, as reported by `channel list`.
    /// Identifies the channel rather than the peer, since a peer may hold
    /// several
    #[serde_as(as = "DisplayFromStr")]
    pub user_channel_id: u128,
    /// Peer the channel is with
    pub pubkey: PublicKey,
    /// On-chain funds to add to the channel, in sat
    pub amount_sat: u64,
}

// --- /ldk/channel/splice-out ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkChannelSpliceOutRequest {
    /// Channel to splice funds out of, as reported by `channel list`.
    /// Identifies the channel rather than the peer, since a peer may hold
    /// several
    #[serde_as(as = "DisplayFromStr")]
    pub user_channel_id: u128,
    /// Peer the channel is with
    pub pubkey: PublicKey,
    /// Destination on-chain address for the spliced-out funds
    pub address: bitcoin::Address<NetworkUnchecked>,
    /// The amount to remove from the channel, in sat; at most the channel's
    /// outbound capacity
    pub amount_sat: u64,
}

// --- /ldk/ln/receive ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkLnReceiveRequest {
    /// The invoice amount in msat
    pub amount_msat: u64,
    /// Seconds until the invoice expires; 3600 if omitted
    #[arg(long)]
    pub expiry_secs: Option<u32>,
    /// The invoice description; empty if omitted
    #[arg(long)]
    pub description: Option<String>,
}

/// An invoice of the LDK node itself.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkLnReceiveResponse {
    /// The bolt11 invoice; paying it funds the LDK node's channel balance,
    /// not a federation
    pub invoice: String,
}

// --- /ldk/ln/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkLnSendRequest {
    /// The bolt11 invoice to pay from the LDK node's own channel balance
    pub invoice: Bolt11Invoice,
}

/// Proof that the invoice was paid. The command waits for the payment to
/// settle and fails if it does not.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkLnSendResponse {
    /// The payment preimage, hex
    pub preimage: String,
}

// --- /ldk/ln/probe ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkLnProbeRequest {
    /// The node to probe a route towards, hex
    pub node_id: PublicKey,
    /// The amount to find paths for, in msat
    pub amount_msat: u64,
}

// --- /ldk/peer/connect ---

#[serde_as]
#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkPeerConnectRequest {
    /// The peer's node id, hex
    pub pubkey: PublicKey,
    /// The peer's `host:port`: an IP, a hostname or an onion address
    #[serde_as(as = "DisplayFromStr")]
    pub host: SocketAddress,
}

// --- /ldk/peer/disconnect ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct LdkPeerDisconnectRequest {
    /// The peer's node id, hex
    pub pubkey: PublicKey,
}

// --- /ldk/peer/list ---

/// Every Lightning peer the LDK node knows, connected or not.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LdkPeerListResponse {
    /// The peers, in no particular order
    pub peers: Vec<PeerInfo>,
}

/// One Lightning peer.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PeerInfo {
    /// The peer's node id, hex
    #[schemars(with = "String")]
    pub node_id: PublicKey,
    /// The socket address the LDK node reaches the peer at
    pub address: String,
    /// Whether the connection is up right now; LDK reconnects on its own
    pub is_connected: bool,
}

// --- /federation/join ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationJoinRequest {
    /// The federation's invite code; the federation has to run on this
    /// gateway's `FM_NETWORK` and carry the lnv2, mintv2 and walletv2 modules
    pub invite: InviteCode,
}

// --- /federation/disable + /federation/enable ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationDisableRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationEnableRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

// --- /federation/list ---

/// The federations the gateway has joined, enabled or not.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationListResponse {
    /// The federations, ordered by id
    pub federations: Vec<FederationInfo>,
}

/// One joined federation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationInfo {
    /// The federation id, hex, which every per-federation command takes
    /// first
    #[schemars(with = "String")]
    pub federation_id: FederationId,
    /// The federation's name from its config; absent if it sets none
    pub federation_name: Option<String>,
}

// --- /federation/config ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationConfigRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

/// The federation's client config as its guardians serve it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationConfigResponse {
    /// The config: name, network, guardian set with their API endpoints,
    /// and every module's consensus parameters. Its shape is the
    /// federation's, not this CLI's
    pub config: serde_json::Value,
}

// --- /federation/balance ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationBalanceRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

/// The gateway's ecash balance in one federation.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationBalanceResponse {
    /// The sum of the notes the gateway holds, in msat
    #[schemars(with = "u64")]
    pub balance_msat: Amount,
}

// --- /federation/module/mint/count ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationMintCountRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

/// The gateway's balance in the federation broken down into the notes that
/// make it up.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationMintCountResponse {
    /// Notes held per denomination, keyed by the denomination's exponent as
    /// a string: key `"10"` counts the notes worth 2^10 msat
    #[schemars(with = "BTreeMap<u8, u64>")]
    pub counts: BTreeMap<Denomination, u64>,
}

// --- /federation/module/mint/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationMintSendRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
    /// The amount with its denomination, e.g. "1000 sat" or "1000000 msat";
    /// rounded up to a multiple of the smallest note, and reissued first when
    /// the notes on hand cannot make it up exactly
    pub amount: Amount,
}

/// The bundle to hand over.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationMintSendResponse {
    /// The ecash as a `fedimint`-prefixed base32 string; its notes have left
    /// the gateway's balance and belong to whoever receives the string first
    pub ecash: String,
}

// --- /federation/module/mint/receive ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationMintReceiveRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
    /// A bundle from any client's ecash send, a `fedimint`-prefixed base32
    /// string; each bundle can be received once per federation
    pub ecash: String,
}

/// The reissue completed. The command waits for the federation to accept
/// the notes and fails if it rejects them.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationMintReceiveResponse {
    /// The amount the bundle was worth, in msat
    #[schemars(with = "u64")]
    pub amount: Amount,
}

// --- /federation/module/wallet/send-fee ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationWalletSendFeeRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

/// What a send costs right now.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationWalletSendFeeResponse {
    /// The miner fee the federation requires for one send transaction, in
    /// sat, from its consensus fee rate. The federation's own per-output fee
    /// comes on top of this when the send is charged
    #[schemars(with = "u64")]
    pub fee: bitcoin::Amount,
}

// --- /federation/module/wallet/send ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationWalletSendRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
    /// The destination address, on the federation's network
    pub address: bitcoin::Address<NetworkUnchecked>,
    /// The amount with its denomination, e.g. "100000 sat"; at least the
    /// federation's dust limit
    pub amount: bitcoin::Amount,
    /// A miner fee with its denomination, e.g. "154 sat", to attach instead
    /// of the one `send-fee` quotes; the federation rejects the send if this
    /// is below what it requires at the time, so only ever raise it
    #[arg(long)]
    pub fee: Option<bitcoin::Amount>,
}

/// The send was broadcast. The command waits for the federation to sign and
/// broadcast the transaction and fails if it does not.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationWalletSendResponse {
    /// Id of the transaction the federation broadcast, hex
    #[schemars(with = "String")]
    pub txid: bitcoin::Txid,
}

// --- /federation/module/wallet/receive ---

#[derive(Debug, Clone, Serialize, Deserialize, Args)]
pub struct FederationWalletReceiveRequest {
    /// The federation id, as printed by `federation list`
    pub federation_id: FederationId,
}

/// Where to send bitcoin to have the federation issue ecash for it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct FederationWalletReceiveResponse {
    /// The gateway's next unused deposit address of the federation's wallet.
    /// A deposit is credited as ecash once the federation sees it confirmed,
    /// less the fee of the transaction that sweeps it into the federation
    /// wallet
    #[schemars(with = "String")]
    pub address: bitcoin::Address<NetworkUnchecked>,
}

// --- errors ---

/// Why an `ldk` command did nothing: LDK refused it, with its reason.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum LdkError {
    #[error("LDK refused: {0}")]
    Ldk(String),
}

/// Why `ldk ln receive` produced no invoice.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum LdkReceiveError {
    #[error("The invoice description is too long")]
    InvalidDescription,
    #[error("LDK refused: {0}")]
    Ldk(String),
}

/// Why `ldk ln send` did not pay.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum LdkSendError {
    #[error("LDK refused: {0}")]
    Ldk(String),
    #[error("The payment failed")]
    PaymentFailed,
}

/// Why a query returned no rows: SQLite refused it, which covers a write
/// statement on the read-only connection as much as a typo.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum QueryError {
    #[error("SQLite rejected the query: {0}")]
    InvalidQuery(String),
}

/// Why `federation join` did not join.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum FederationJoinError {
    #[error("The federation's config could not be downloaded: {0}")]
    ConfigUnavailable(String),
    #[error("The federation is missing the required {0} module")]
    MissingModule(String),
}

/// Why a per-federation command did nothing: the federation is not joined,
/// or its client failed to load.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum NotJoinedError {
    #[error("The federation is not joined")]
    NotJoined,
}

/// Why `federation module mint send` produced no ecash.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum MintSendError {
    #[error(transparent)]
    NotJoined(#[from] NotJoinedError),
    #[error(transparent)]
    Send(#[from] SendECashError),
}

/// Why `federation module mint receive` did not reissue the ecash.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum MintReceiveError {
    #[error(transparent)]
    NotJoined(#[from] NotJoinedError),
    #[error("The ecash is not a fedimint-prefixed base32 bundle: {0}")]
    InvalidEcash(String),
    #[error(transparent)]
    Receive(#[from] ReceiveECashError),
    #[error("The federation rejected the notes as already spent")]
    Rejected,
}

/// Why `federation module wallet send-fee` quoted nothing.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum WalletSendFeeError {
    #[error(transparent)]
    NotJoined(#[from] NotJoinedError),
    #[error(transparent)]
    Wallet(#[from] fedimint_walletv2_client::SendError),
}

/// Why `federation module wallet send` broadcast nothing.
#[derive(Error, Debug, Clone, Eq, PartialEq, ErrorCode)]
pub enum WalletSendError {
    #[error(transparent)]
    NotJoined(#[from] NotJoinedError),
    #[error(transparent)]
    Wallet(#[from] fedimint_walletv2_client::SendError),
    #[error("The federation aborted the send")]
    Aborted,
    #[error("The send failed")]
    Failure,
}
