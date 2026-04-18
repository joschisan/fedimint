//! Freestanding API handlers for [`crate::consensus::api::ConsensusApi`].

use picomint_api_client::config::ConsensusConfig;
use picomint_api_client::transaction::{Transaction, TransactionError};
use picomint_core::module::ApiError;

use crate::consensus::api::ConsensusApi;

pub async fn submit_transaction(
    api: &ConsensusApi,
    tx: Transaction,
) -> Result<Result<(), TransactionError>, ApiError> {
    Ok(api.submit_transaction(tx).await)
}

pub async fn client_config(api: &ConsensusApi, _: ()) -> Result<ConsensusConfig, ApiError> {
    Ok(api.client_cfg.clone())
}

pub async fn liveness(_: &ConsensusApi, _: ()) -> Result<(), ApiError> {
    Ok(())
}
