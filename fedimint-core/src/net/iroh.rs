use std::net::SocketAddr;

use iroh::endpoint::presets::N0;
use iroh::{Endpoint, SecretKey};

/// Build an iroh endpoint for server-side usage (P2P and API).
///
/// Uses N0 defaults (relay + discovery).
pub async fn build_iroh_endpoint(
    secret_key: SecretKey,
    bind_addr: SocketAddr,
    alpn: &[u8],
) -> Result<Endpoint, anyhow::Error> {
    let endpoint = Endpoint::builder(N0)
        .secret_key(secret_key)
        .alpns(vec![alpn.to_vec()])
        .bind_addr(bind_addr)?
        .bind()
        .await?;

    Ok(endpoint)
}
