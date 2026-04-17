//! Freestanding API handlers for [`crate::consensus::api::ConsensusApi`].

use picomint_api_client::config::ConsensusConfig;
use picomint_api_client::transaction::{Transaction, TransactionSubmissionOutcome};
use picomint_core::TransactionId;
use picomint_core::module::ApiError;

use crate::consensus::api::ConsensusApi;

pub async fn submit_transaction(
    api: &ConsensusApi,
    tx: Transaction,
) -> Result<TransactionSubmissionOutcome, ApiError> {
    Ok(TransactionSubmissionOutcome(api.submit_transaction(tx).await))
}

pub async fn await_transaction(
    api: &ConsensusApi,
    tx_hash: TransactionId,
) -> Result<TransactionId, ApiError> {
    api.await_transaction(tx_hash).await;
    Ok(tx_hash)
}

pub async fn client_config(api: &ConsensusApi, _: ()) -> Result<ConsensusConfig, ApiError> {
    Ok(api.client_cfg.clone())
}

pub async fn liveness(_: &ConsensusApi, _: ()) -> Result<(), ApiError> {
    Ok(())
}
