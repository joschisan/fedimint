//! Freestanding API handlers for [`crate::consensus::api::ConsensusApi`].

use picomint_core::config::ConsensusConfig;
use picomint_core::module::ApiError;
use picomint_core::transaction::{Transaction, TransactionError};

use crate::consensus::api::ConsensusApi;

pub async fn submit_transaction(
    api: &ConsensusApi,
    tx: Transaction,
) -> Result<Result<(), TransactionError>, ApiError> {
    Ok(api.submit_transaction(tx).await)
}

pub fn client_config(api: &ConsensusApi, (): ()) -> Result<ConsensusConfig, ApiError> {
    Ok(api.client_cfg.clone())
}

pub fn liveness(_: &ConsensusApi, (): ()) -> Result<(), ApiError> {
    Ok(())
}
