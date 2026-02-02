use bech32::{Bech32, Hrp};
use lightning_invoice::Bolt11Invoice;
use serde::{Deserialize, Serialize};
use serde_with::hex::Hex;
use serde_with::serde_as;
use url::Url;

/// Generic LNURL response wrapper that handles the status field.
/// All LNURL responses follow the {"status": "OK"|"ERROR", ...} pattern.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status")]
pub enum LnurlResponse<T> {
    #[serde(rename = "OK")]
    Ok(T),
    #[serde(rename = "ERROR")]
    Error { reason: String },
}

impl<T> LnurlResponse<T> {
    pub fn into_result(self) -> Result<T, String> {
        match self {
            Self::Ok(data) => Ok(data),
            Self::Error { reason } => Err(reason),
        }
    }
}

/// Decode a bech32-encoded LNURL string to a URL
pub fn parse_lnurl(s: &str) -> Option<Url> {
    let (hrp, data) = bech32::decode(s).ok()?;

    if hrp.as_str() != "lnurl" {
        return None;
    }

    String::from_utf8(data)
        .ok()
        .and_then(|s| Url::parse(&s).ok())
}

/// Encode a URL as a bech32 LNURL string
pub fn encode_lnurl(url: &Url) -> String {
    bech32::encode::<Bech32>(
        Hrp::parse("lnurl").expect("valid hrp"),
        url.as_str().as_bytes(),
    )
    .expect("encoding succeeds")
}

/// Parse a lightning address (user@domain) to its LNURL-pay endpoint URL
pub fn parse_address(s: &str) -> Option<Url> {
    let (user, domain) = s.split_once('@')?;

    if user.is_empty() || domain.is_empty() {
        return None;
    }

    Url::parse(&format!("https://{domain}/.well-known/lnurlp/{user}")).ok()
}

pub fn pay_request_tag() -> String {
    "payRequest".to_string()
}

/// LNURL-pay response (LUD-06)
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PayResponse {
    pub tag: String,
    pub callback: String,
    pub metadata: String,
    pub min_sendable: u64,
    pub max_sendable: u64,
}

/// Response when requesting an invoice from LNURL-pay callback
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InvoiceResponse {
    /// The BOLT11 invoice
    pub pr: Bolt11Invoice,
    /// LUD-21 verify URL
    pub verify: Option<String>,
}

/// LUD-21 verify response
#[serde_as]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VerifyResponse {
    pub settled: bool,
    #[serde_as(as = "Option<Hex>")]
    pub preimage: Option<[u8; 32]>,
}

/// Fetch and parse an LNURL-pay response
pub async fn request(url: &Url) -> Result<PayResponse, String> {
    reqwest::get(url.clone())
        .await
        .map_err(|e| e.to_string())?
        .json::<LnurlResponse<PayResponse>>()
        .await
        .map_err(|e| e.to_string())?
        .into_result()
}

/// Fetch an invoice from an LNURL-pay callback
pub async fn get_invoice(
    response: &PayResponse,
    amount_msat: u64,
) -> Result<InvoiceResponse, String> {
    if amount_msat < response.min_sendable {
        return Err("Amount too low".to_string());
    }

    if amount_msat > response.max_sendable {
        return Err("Amount too high".to_string());
    }

    let callback_url = format!("{}?amount={}", response.callback, amount_msat);

    reqwest::get(callback_url)
        .await
        .map_err(|e| e.to_string())?
        .json::<LnurlResponse<InvoiceResponse>>()
        .await
        .map_err(|e| e.to_string())?
        .into_result()
}

/// Verify a payment using LUD-21
pub async fn verify_invoice(url: &str) -> Result<VerifyResponse, String> {
    reqwest::get(url)
        .await
        .map_err(|e| e.to_string())?
        .json::<LnurlResponse<VerifyResponse>>()
        .await
        .map_err(|e| e.to_string())?
        .into_result()
}
