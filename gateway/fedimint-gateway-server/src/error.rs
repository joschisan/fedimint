use std::fmt::Display;

use axum::Json;
use axum::body::Body;
use axum::response::{IntoResponse, Response};
use fedimint_core::config::FederationIdPrefix;
use reqwest::StatusCode;
use thiserror::Error;

/// Simple error type for admin/CLI endpoints.
#[derive(Debug)]
pub struct CliError {
    pub code: StatusCode,
    pub error: String,
}

impl Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.error)
    }
}

impl std::error::Error for CliError {}

impl CliError {
    pub fn bad_request(error: impl Display) -> Self {
        Self {
            code: StatusCode::BAD_REQUEST,
            error: error.to_string(),
        }
    }

    pub fn internal(error: impl Display) -> Self {
        Self {
            code: StatusCode::INTERNAL_SERVER_ERROR,
            error: error.to_string(),
        }
    }
}

impl IntoResponse for CliError {
    fn into_response(self) -> axum::response::Response {
        (self.code, self.error).into_response()
    }
}

impl From<FederationNotConnected> for CliError {
    fn from(e: FederationNotConnected) -> Self {
        Self::bad_request(e)
    }
}

impl From<fedimint_gateway_common::LightningRpcError> for CliError {
    fn from(e: fedimint_gateway_common::LightningRpcError) -> Self {
        Self::internal(e)
    }
}

impl From<anyhow::Error> for CliError {
    fn from(e: anyhow::Error) -> Self {
        Self::internal(e)
    }
}

/// Public error that indicates the requested federation is not connected to
/// this gateway.
#[derive(Debug, Error)]
pub struct FederationNotConnected {
    pub federation_id_prefix: FederationIdPrefix,
}

impl Display for FederationNotConnected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "No federation available for prefix {}",
            self.federation_id_prefix
        )
    }
}

/// LNURL-compliant error response for verify endpoints
#[derive(Debug, Error)]
pub(crate) struct LnurlError {
    code: StatusCode,
    reason: anyhow::Error,
}

impl Display for LnurlError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "LNURL Error: {}", self.reason,)
    }
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
