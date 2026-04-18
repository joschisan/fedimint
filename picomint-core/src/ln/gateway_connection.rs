use std::sync::Arc;

use crate::util::SafeUrl;
use reqwest::Method;
use serde::Serialize;
use serde::de::DeserializeOwned;
use thiserror::Error;

/// Error type for gateway API calls
#[derive(Debug, Error)]
pub enum GatewayError {
    #[error("Invalid response: {0}")]
    InvalidResponse(anyhow::Error),

    #[error("Server error: {0}")]
    ServerError(anyhow::Error),

    #[error("Connection error: {0}")]
    Connection(anyhow::Error),
}

#[derive(Clone, Debug)]
pub struct GatewayApi {
    client: Arc<reqwest::Client>,
}

impl GatewayApi {
    pub fn new() -> Self {
        Self {
            client: Arc::new(reqwest::Client::new()),
        }
    }

    pub async fn request<P: Serialize, T: DeserializeOwned>(
        &self,
        base_url: &SafeUrl,
        method: Method,
        route: &str,
        payload: Option<P>,
    ) -> Result<T, GatewayError> {
        let url = base_url.join(route).expect("Invalid base url");
        let mut builder = self.client.request(method, url.to_unsafe());
        if let Some(payload) = payload {
            builder = builder.json(&payload);
        }

        let response = builder
            .send()
            .await
            .map_err(|e| GatewayError::Connection(e.into()))?;

        match response.status() {
            reqwest::StatusCode::OK => {
                let value = response
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|e| GatewayError::InvalidResponse(e.into()))?;
                serde_json::from_value::<T>(value).map_err(|e| {
                    GatewayError::InvalidResponse(anyhow::anyhow!("Received invalid response: {e}"))
                })
            }
            status => Err(GatewayError::ServerError(anyhow::anyhow!(
                "HTTP request returned unexpected status: {status}"
            ))),
        }
    }
}
