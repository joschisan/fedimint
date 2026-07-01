use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use fedimint_core::crit;
use fedimint_core::util::FmtCompactAnyhow;
use fedimint_logging::LOG_GATEWAY;
use reqwest::StatusCode;

/// Error type for the gateway's HTTP and admin-socket handlers. Wraps
/// `anyhow::Error` and responds with `500` plus the error message. The public
/// routes are the LNv2 protocol, whose clients only branch on success vs
/// failure, so there is no per-category status code or message redaction.
#[derive(Debug)]
pub struct GatewayError(anyhow::Error);

impl<E> From<E> for GatewayError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// `Display` (not `Error`) so `#[instrument(err)]` can render it, without making
// `GatewayError: Into<anyhow::Error>`, which would clash with the blanket
// `From` impl above.
impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        crit!(target: LOG_GATEWAY, err = %self.0.fmt_compact_anyhow(), "Gateway request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

/// LNURL-compliant error envelope for the verify endpoint. This is the LNURL
/// wire format (`{"status":"ERROR","reason":...}`), not error redaction, so it
/// is kept distinct from [`GatewayError`].
#[derive(Debug)]
pub(crate) struct LnurlError {
    code: StatusCode,
    reason: anyhow::Error,
}

impl LnurlError {
    pub(crate) fn internal(reason: anyhow::Error) -> Self {
        Self {
            code: StatusCode::INTERNAL_SERVER_ERROR,
            reason,
        }
    }
}

impl IntoResponse for LnurlError {
    fn into_response(self) -> Response<Body> {
        let json = Json(serde_json::json!({
            "status": "ERROR",
            "reason": self.reason.to_string(),
        }));

        (self.code, json).into_response()
    }
}
