//! Client-side HTTP calls to the gateway daemon. The wire types
//! (`RoutingInfo`, `PaymentFee`, `*Payload`) live in
//! `picomint_core::ln::gateway_api` because the gateway daemon must agree
//! on them; the request *helpers* below are client-only.

use bitcoin::secp256k1::schnorr::Signature;
use lightning_invoice::Bolt11Invoice;
use picomint_core::config::FederationId;
use picomint_core::ln::contracts::{IncomingContract, OutgoingContract};
use picomint_core::ln::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT, SEND_PAYMENT_ENDPOINT,
};
use picomint_core::ln::gateway_api::{CreateBolt11InvoicePayload, RoutingInfo, SendPaymentPayload};
use picomint_core::ln::{Bolt11InvoiceDescription, LightningInvoice};
use picomint_core::util::SafeUrl;
use picomint_core::{Amount, OutPoint};
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("Could not connect to the gateway")]
    Connection,

    #[error("Gateway returned an unexpected status")]
    UnexpectedStatus,

    #[error("Failed to parse gateway response")]
    InvalidResponse,
}

/// One-shot HTTP request to a gateway endpoint. The gateway is a
/// human-driven web service we hit a handful of times per Lightning
/// operation, so a fresh `reqwest::Client` per call is fine — there is no
/// long-lived stream of requests to amortize a shared client over.
async fn request<P: Serialize, T: DeserializeOwned>(
    base_url: &SafeUrl,
    method: Method,
    route: &str,
    payload: Option<P>,
) -> Result<T, GatewayError> {
    let url = base_url.join(route).expect("Invalid base url");
    let mut builder = reqwest::Client::new().request(method, url.to_unsafe());
    if let Some(payload) = payload {
        builder = builder.json(&payload);
    }

    let response = builder.send().await.map_err(|_| GatewayError::Connection)?;

    if response.status() != reqwest::StatusCode::OK {
        return Err(GatewayError::UnexpectedStatus);
    }

    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|_| GatewayError::InvalidResponse)?;
    serde_json::from_value(value).map_err(|_| GatewayError::InvalidResponse)
}

pub async fn routing_info(
    gateway_api: SafeUrl,
    federation_id: &FederationId,
) -> Result<Option<RoutingInfo>, GatewayError> {
    request(
        &gateway_api,
        Method::POST,
        ROUTING_INFO_ENDPOINT,
        Some(federation_id),
    )
    .await
}

pub async fn bolt11_invoice(
    gateway_api: SafeUrl,
    federation_id: FederationId,
    contract: IncomingContract,
    amount: Amount,
    description: Bolt11InvoiceDescription,
    expiry_secs: u32,
) -> Result<Bolt11Invoice, GatewayError> {
    request(
        &gateway_api,
        Method::POST,
        CREATE_BOLT11_INVOICE_ENDPOINT,
        Some(CreateBolt11InvoicePayload {
            federation_id,
            contract,
            amount,
            description,
            expiry_secs,
        }),
    )
    .await
}

pub async fn send_payment(
    gateway_api: SafeUrl,
    federation_id: FederationId,
    outpoint: OutPoint,
    contract: OutgoingContract,
    invoice: LightningInvoice,
    auth: Signature,
) -> Result<Result<[u8; 32], Signature>, GatewayError> {
    request(
        &gateway_api,
        Method::POST,
        SEND_PAYMENT_ENDPOINT,
        Some(SendPaymentPayload {
            federation_id,
            outpoint,
            contract,
            invoice,
            auth,
        }),
    )
    .await
}
