use std::net::SocketAddr;

use anyhow::{anyhow, bail, ensure};
use axum::extract::{Path, Query};
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use bitcoin::secp256k1::{self, Keypair, PublicKey, ecdh};
use clap::Parser;
use lightning_invoice::Bolt11Invoice;
use picomint_core::Amount;
use picomint_core::config::FederationId;
use picomint_core::ln::contracts::{IncomingContract, PaymentImage};
use picomint_core::ln::endpoint_constants::{
    CREATE_BOLT11_INVOICE_ENDPOINT, ROUTING_INFO_ENDPOINT,
};
use picomint_core::ln::gateway_api::{CreateBolt11InvoicePayload, PaymentFee, RoutingInfo};
use picomint_core::ln::lnurl::LnurlRequest;
use picomint_core::ln::{
    Bolt11InvoiceDescription, IncomingContractPath, MINIMUM_INCOMING_CONTRACT_AMOUNT,
};
use picomint_core::secret::Secret;
use picomint_core::time::duration_since_epoch;
use picomint_core::util::SafeUrl;
use picomint_encoding::Encodable;
use picomint_lnurl::{InvoiceResponse, LnurlResponse, PayResponse, pay_request_tag};
use picomint_logging::TracingSetup;
use reqwest::Method;
use serde::Deserialize;
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::net::TcpListener;
use tower_http::cors;
use tower_http::cors::CorsLayer;
use tpe::AggregatePublicKey;
use tracing::info;

const MAX_SENDABLE_MSAT: u64 = 100_000_000_000;
const MIN_SENDABLE_MSAT: u64 = 100_000;

#[derive(Debug, Parser)]
struct CliOpts {
    /// Public HTTP API listen address. Should be open in the firewall —
    /// wallets and the paying side hit this directly.
    #[arg(long, env = "API_ADDR", default_value = "0.0.0.0:8080")]
    api_addr: SocketAddr,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    TracingSetup::default().init()?;

    let cli_opts = CliOpts::parse();

    let cors = CorsLayer::new()
        .allow_origin(cors::Any)
        .allow_methods(cors::Any)
        .allow_headers(cors::Any);

    let app = Router::new()
        .route("/", get(health_check))
        .route("/pay/{payload}", get(pay))
        .route("/invoice/{payload}", get(invoice))
        .layer(cors);

    info!(api_addr = %cli_opts.api_addr, "recurring-daemon started");

    let listener = TcpListener::bind(cli_opts.api_addr).await?;

    axum::serve(listener, app).await?;

    Ok(())
}

async fn health_check(headers: HeaderMap) -> impl IntoResponse {
    format!(
        "recurring-daemon is up and running at {}",
        base_url(&headers)
    )
}

fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("x-forwarded-host")
        .or_else(|| headers.get("host"))
        .and_then(|h| h.to_str().ok())
        .unwrap_or("localhost");

    let scheme = headers
        .get("x-forwarded-proto")
        .and_then(|h| h.to_str().ok())
        .unwrap_or("http");

    format!("{scheme}://{host}/")
}

async fn pay(headers: HeaderMap, Path(payload): Path<String>) -> Json<LnurlResponse<PayResponse>> {
    Json(LnurlResponse::Ok(PayResponse {
        callback: format!("{}invoice/{payload}", base_url(&headers)),
        max_sendable: MAX_SENDABLE_MSAT,
        min_sendable: MIN_SENDABLE_MSAT,
        tag: pay_request_tag(),
        metadata: "[[\"text/plain\", \"Pay to Recurringd\"]]".to_string(),
    }))
}

#[derive(Debug, Serialize, Deserialize)]
struct GetInvoiceParams {
    amount: u64,
}

async fn invoice(
    Path(payload): Path<String>,
    Query(params): Query<GetInvoiceParams>,
) -> Json<LnurlResponse<InvoiceResponse>> {
    let Ok(request) = picomint_base32::decode::<LnurlRequest>(&payload) else {
        return Json(LnurlResponse::error("Failed to decode payload"));
    };

    if params.amount < MIN_SENDABLE_MSAT || params.amount > MAX_SENDABLE_MSAT {
        return Json(LnurlResponse::error(format!(
            "Amount must be between {} and {}",
            MIN_SENDABLE_MSAT, MAX_SENDABLE_MSAT
        )));
    }

    let (gateway, invoice) = match create_contract_and_fetch_invoice(
        request.federation_id,
        request.recipient_pk,
        request.aggregate_pk,
        request.gateways,
        params.amount,
        3600, // standard expiry time of one hour
    )
    .await
    {
        Ok(result) => result,
        Err(e) => {
            return Json(LnurlResponse::error(e.to_string()));
        }
    };

    info!(%params.amount, %gateway, "Created invoice");

    let verify = gateway
        .join(&format!("verify/{}", invoice.payment_hash()))
        .expect("verify/{hash} is a valid relative path");

    Json(LnurlResponse::Ok(InvoiceResponse {
        pr: invoice.clone(),
        verify: Some(verify.to_string()),
    }))
}

async fn create_contract_and_fetch_invoice(
    federation_id: FederationId,
    recipient_pk: PublicKey,
    aggregate_pk: AggregatePublicKey,
    gateways: Vec<SafeUrl>,
    amount: u64,
    expiry_secs: u32,
) -> anyhow::Result<(SafeUrl, Bolt11Invoice)> {
    let ephemeral_keypair = Keypair::new(secp256k1::SECP256K1, &mut rand::thread_rng());

    let shared_secret =
        ecdh::SharedSecret::new(&recipient_pk, &ephemeral_keypair.secret_key()).secret_bytes();

    let contract_secret = Secret::new_root(&shared_secret);

    let encryption_seed = contract_secret
        .child(&IncomingContractPath::EncryptionSeed)
        .to_bytes();

    let preimage = contract_secret
        .child(&IncomingContractPath::Preimage)
        .to_bytes();

    let claim_tweak = contract_secret
        .child(&IncomingContractPath::ClaimKey)
        .to_secp_scalar();

    let claim_pk = recipient_pk
        .mul_tweak(secp256k1::SECP256K1, &claim_tweak)
        .expect("Tweak is valid");

    let (routing_info, gateway) = select_gateway(gateways, federation_id).await?;

    ensure!(
        routing_info.receive_fee.le(&PaymentFee::RECEIVE_FEE_LIMIT),
        "Payment fee exceeds limit"
    );

    let contract_amount = routing_info.receive_fee.subtract_from(amount);

    ensure!(
        contract_amount >= MINIMUM_INCOMING_CONTRACT_AMOUNT,
        "Amount too small"
    );

    let expiration = duration_since_epoch()
        .as_secs()
        .saturating_add(u64::from(expiry_secs));

    let contract = IncomingContract::new(
        aggregate_pk,
        encryption_seed,
        preimage,
        PaymentImage::Hash(preimage.consensus_hash()),
        contract_amount,
        expiration,
        claim_pk,
        routing_info.module_public_key,
        ephemeral_keypair.public_key(),
    );

    let invoice: Bolt11Invoice = gateway_request(
        &gateway,
        CREATE_BOLT11_INVOICE_ENDPOINT,
        &CreateBolt11InvoicePayload {
            federation_id,
            contract: contract.clone(),
            amount: Amount::from_msats(amount),
            description: Bolt11InvoiceDescription::Direct("LNURL Payment".to_string()),
            expiry_secs,
        },
    )
    .await?;

    ensure!(
        invoice.payment_hash() == &preimage.consensus_hash(),
        "Invalid invoice payment hash"
    );

    ensure!(
        invoice.amount_milli_satoshis() == Some(amount),
        "Invalid invoice amount"
    );

    Ok((gateway, invoice))
}

async fn select_gateway(
    gateways: Vec<SafeUrl>,
    federation_id: FederationId,
) -> anyhow::Result<(RoutingInfo, SafeUrl)> {
    for gateway in gateways {
        if let Ok(routing_info) = gateway_request::<_, Option<RoutingInfo>>(
            &gateway,
            ROUTING_INFO_ENDPOINT,
            &federation_id,
        )
        .await
            && let Some(routing_info) = routing_info
        {
            return Ok((routing_info, gateway));
        }
    }

    bail!("All gateways are offline or do not support this federation")
}

/// One-shot POST to a gateway endpoint with a JSON payload, returning the
/// JSON-decoded response.
async fn gateway_request<P: Serialize, T: DeserializeOwned>(
    base_url: &SafeUrl,
    route: &str,
    payload: &P,
) -> anyhow::Result<T> {
    let url = base_url.join(route).expect("Invalid base url");

    let response = reqwest::Client::new()
        .request(Method::POST, url.to_unsafe())
        .json(payload)
        .send()
        .await
        .map_err(|e| anyhow!("Could not connect to gateway: {e}"))?;

    if response.status() != reqwest::StatusCode::OK {
        bail!(
            "Gateway returned an unexpected status: {}",
            response.status()
        );
    }

    response
        .json()
        .await
        .map_err(|e| anyhow!("Failed to parse gateway response: {e}"))
}
