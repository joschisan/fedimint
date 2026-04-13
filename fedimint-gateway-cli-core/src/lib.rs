use std::fmt;

use bitcoin::address::NetworkUnchecked;
use fedimint_core::config::FederationId;
use fedimint_core::{Amount, BitcoinAmountOrAll, secp256k1};
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
pub const ROUTE_FED_JOIN: &str = "/federation/join";
pub const ROUTE_FED_LIST: &str = "/federation/list";
pub const ROUTE_FED_CONFIG: &str = "/federation/config";
pub const ROUTE_FED_INVITE: &str = "/federation/invite";

// Per-federation module commands
pub const ROUTE_MODULE_MINT_COUNT: &str = "/module/mintv2/count";
pub const ROUTE_MODULE_MINT_SEND: &str = "/module/mintv2/send";
pub const ROUTE_MODULE_MINT_RECEIVE: &str = "/module/mintv2/receive";
pub const ROUTE_MODULE_WALLET_INFO: &str = "/module/walletv2/info";
pub const ROUTE_MODULE_WALLET_SEND_FEE: &str = "/module/walletv2/send-fee";
pub const ROUTE_MODULE_WALLET_SEND: &str = "/module/walletv2/send";
pub const ROUTE_MODULE_WALLET_RECEIVE: &str = "/module/walletv2/receive";

// Request types

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConnectFedPayload {
    pub invite_code: String,
    pub recover: Option<bool>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ConfigPayload {
    pub federation_id: Option<FederationId>,
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
    pub address: bitcoin::Address<NetworkUnchecked>,
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
pub struct PeerConnectRequest {
    pub pubkey: secp256k1::PublicKey,
    pub host: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct PeerDisconnectRequest {
    pub pubkey: secp256k1::PublicKey,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ListTransactionsPayload {
    pub start_secs: u64,
    pub end_secs: u64,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct SpendEcashPayload {
    pub federation_id: FederationId,
    pub amount: Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ReceiveEcashPayload {
    pub notes: String,
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct DepositAddressPayload {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleMintCountRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleMintSendRequest {
    pub federation_id: FederationId,
    pub amount: Amount,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleMintReceiveRequest {
    pub federation_id: FederationId,
    pub ecash: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleWalletInfoRequest {
    pub federation_id: FederationId,
    pub subcommand: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleWalletSendFeeRequest {
    pub federation_id: FederationId,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleWalletSendRequest {
    pub federation_id: FederationId,
    pub address: bitcoin::Address<bitcoin::address::NetworkUnchecked>,
    pub amount: bitcoin::Amount,
    pub fee: Option<bitcoin::Amount>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct ModuleWalletReceiveRequest {
    pub federation_id: FederationId,
}
