//! Connection primitives for fedimint API clients.
//!
//! [`FederationApi`](crate::api::FederationApi) owns the per-URL connection
//! tasks; this module just provides the error type, the `IGuardianConnection`
//! trait, and the iroh transport impl.

use std::fmt::Debug;
use std::str::FromStr;
use anyhow::{Context, bail};
use fedimint_core::config::ALEPH_BFT_UNIT_BYTE_LIMIT;
use fedimint_core::module::{ApiError, ApiMethod, ApiRequestErased, IrohApiRequest};
use fedimint_core::util::SafeUrl;
use fedimint_core::PeerId;
use fedimint_logging::LOG_CLIENT_NET_API;
use iroh::endpoint::Connection;
use iroh::PublicKey;
use serde_json::Value;
use thiserror::Error;
use tracing::{trace, warn};

// ── Error types ─────────────────────────────────────────────────────────────

/// An API request error when calling a single federation peer
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum ServerError {
    #[error("Response deserialization error: {0}")]
    ResponseDeserialization(anyhow::Error),

    #[error("Invalid peer id: {peer_id}")]
    InvalidPeerId { peer_id: PeerId },

    #[error("Invalid peer url: {url}")]
    InvalidPeerUrl { url: SafeUrl, source: anyhow::Error },

    #[error("Invalid endpoint")]
    InvalidEndpoint(anyhow::Error),

    #[error("Connection failed: {0}")]
    Connection(anyhow::Error),

    #[error("Transport error: {0}")]
    Transport(anyhow::Error),

    #[error("Invalid rpc id")]
    InvalidRpcId(anyhow::Error),

    #[error("Invalid request")]
    InvalidRequest(anyhow::Error),

    #[error("Invalid response: {0}")]
    InvalidResponse(anyhow::Error),

    #[error("Unspecified server error: {0}")]
    ServerError(anyhow::Error),

    #[error("Unspecified condition error: {0}")]
    ConditionFailed(anyhow::Error),

    #[error("Unspecified internal client error: {0}")]
    InternalClientError(anyhow::Error),
}

impl ServerError {
    pub fn is_unusual(&self) -> bool {
        match self {
            ServerError::ResponseDeserialization(_)
            | ServerError::InvalidPeerId { .. }
            | ServerError::InvalidPeerUrl { .. }
            | ServerError::InvalidResponse(_)
            | ServerError::InvalidRpcId(_)
            | ServerError::InvalidRequest(_)
            | ServerError::InternalClientError(_)
            | ServerError::InvalidEndpoint(_)
            | ServerError::ServerError(_) => true,
            ServerError::Connection(_)
            | ServerError::Transport(_)
            | ServerError::ConditionFailed(_) => false,
        }
    }

    pub fn report_if_unusual(&self, peer_id: PeerId, context: &str) {
        let unusual = self.is_unusual();

        trace!(target: LOG_CLIENT_NET_API, error = %self, %context, "ServerError");

        if unusual {
            warn!(target: LOG_CLIENT_NET_API, error = %self, %context, %peer_id, "Unusual ServerError");
        }
    }
}

pub type ServerResult<T> = Result<T, ServerError>;

// ── Connection trait ────────────────────────────────────────────────────────

#[async_trait::async_trait]
pub trait IGuardianConnection: Debug + Send + Sync + 'static {
    async fn request(&self, method: ApiMethod, request: ApiRequestErased) -> ServerResult<Value>;
}

// ── Iroh helpers ────────────────────────────────────────────────────────────

const IROH_MAX_RESPONSE_BYTES: usize = ALEPH_BFT_UNIT_BYTE_LIMIT * 3600 * 4 * 2;

/// Parse a node ID from an iroh:// URL.
pub fn node_id_from_url(url: &SafeUrl) -> anyhow::Result<PublicKey> {
    if url.scheme() != "iroh" {
        bail!("Unsupported scheme: {}, expected iroh://", url.scheme());
    }
    let host = url.host_str().context("Missing host string in Iroh URL")?;
    PublicKey::from_str(host).context("Failed to parse node id")
}

#[async_trait::async_trait]
impl IGuardianConnection for Connection {
    async fn request(&self, method: ApiMethod, request: ApiRequestErased) -> ServerResult<Value> {
        let json = serde_json::to_vec(&IrohApiRequest { method, request })
            .expect("Serialization to vec can't fail");

        let (mut sink, mut stream) = self
            .open_bi()
            .await
            .map_err(|e| ServerError::Transport(e.into()))?;

        sink.write_all(&json)
            .await
            .map_err(|e| ServerError::Transport(e.into()))?;

        sink.finish()
            .map_err(|e| ServerError::Transport(e.into()))?;

        let response = stream
            .read_to_end(IROH_MAX_RESPONSE_BYTES)
            .await
            .map_err(|e| ServerError::Transport(e.into()))?;

        let response = serde_json::from_slice::<Result<Value, ApiError>>(&response)
            .map_err(|e| ServerError::InvalidResponse(e.into()))?;

        response.map_err(|e| ServerError::InvalidResponse(anyhow::anyhow!("Api Error: {:?}", e)))
    }
}
