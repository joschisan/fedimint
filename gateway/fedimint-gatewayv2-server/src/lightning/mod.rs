//! LDK-backed lightning integration for `gatewaydv2`.
//!
//! This is a trimmed, LDK-only copy of the former `fedimint-lightning`
//! interface. `gatewaydv2` owns its lightning backend directly
//! ([`GatewayLdkClient`]) instead of going through the old `ILnRpcClient` trait
//! abstraction (there is only one backend, so the indirection bought nothing).
//!
//! The few types that are shared with `fedimint-gwv2-client` across the
//! `IGatewayClientV2` boundary — `InterceptPaymentResponse`, `PaymentAction`,
//! `Preimage`, `LightningRpcError` — still come from `fedimint-lightning`.

pub mod ldk;

use std::sync::Arc;

use bitcoin::Network;
use bitcoin::hashes::sha256;
use fedimint_core::secp256k1::PublicKey;
use fedimint_gateway_common::ChannelInfo;
use fedimint_lightning::Preimage;
pub use ldk::GatewayLdkClient;
use serde::{Deserialize, Serialize};

/// Represents an active connection to the lightning node.
#[derive(Clone, Debug)]
pub struct LightningContext {
    pub lnrpc: Arc<GatewayLdkClient>,
    pub lightning_public_key: PublicKey,
    pub lightning_alias: String,
    pub lightning_network: Network,
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
