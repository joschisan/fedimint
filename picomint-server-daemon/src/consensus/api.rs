//! Implements the client API through which users interact with the federation

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use picomint_api_client::config::ConsensusConfig;
use picomint_api_client::transaction::{ConsensusItem, Transaction, TransactionError};
use picomint_bitcoin_rpc::BitcoinRpcMonitor;
use picomint_core::endpoint_constants::{
    AWAIT_TRANSACTION_ENDPOINT, CLIENT_CONFIG_ENDPOINT, LIVENESS_ENDPOINT,
    SUBMIT_TRANSACTION_ENDPOINT,
};
use picomint_core::module::audit::{Audit, AuditSummary};
use picomint_core::module::{ApiError, ApiRequestErased};
use picomint_server_core::handler;

use crate::consensus::rpc;
use picomint_core::task::TaskGroup;
use picomint_core::{PeerId, TransactionId};
use picomint_logging::LOG_NET_API;
use picomint_redb::{Database, ReadTransaction};
use tokio::sync::watch::{self, Receiver, Sender};
use tracing::{debug, warn};

use crate::config::ServerConfig;
use crate::consensus::db::ACCEPTED_TRANSACTION;
use crate::consensus::engine::get_finished_session_count_static;
use crate::consensus::server::{Server, process_transaction_with_server};
use crate::p2p::P2PStatusReceivers;

#[derive(Clone)]
pub struct ConsensusApi {
    /// Our server configuration
    pub cfg: ServerConfig,
    /// Database for serving the API
    pub db: Database,
    /// Static wire-dispatch handle to the fixed module set
    pub server: Server,
    /// Cached client config
    pub client_cfg: ConsensusConfig,
    /// For sending API events to consensus such as transactions
    pub submission_sender: async_channel::Sender<ConsensusItem>,
    pub shutdown_receiver: Receiver<Option<u64>>,
    pub shutdown_sender: Sender<Option<u64>>,
    pub ord_latency_receiver: watch::Receiver<Option<Duration>>,
    pub p2p_status_receivers: P2PStatusReceivers,
    pub ci_status_receivers: BTreeMap<PeerId, Receiver<Option<u64>>>,
    pub bitcoin_rpc_connection: BitcoinRpcMonitor,
    pub task_group: TaskGroup,
}

impl ConsensusApi {
    // Returns an error if and only if the submitted transaction is invalid
    // and will be rejected if we were to submit it to consensus.
    pub async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<TransactionId, TransactionError> {
        let txid = transaction.tx_hash();

        debug!(target: LOG_NET_API, %txid, "Received a submitted transaction");

        // Create write tx — we only use it to verify the transaction is valid;
        // dropped without commit so no state is mutated.
        let tx = self.db.begin_write().await;
        if tx.get(&ACCEPTED_TRANSACTION, &txid).is_some() {
            debug!(target: LOG_NET_API, %txid, "Transaction already accepted");
            return Ok(txid);
        }

        process_transaction_with_server(&self.server, &tx, &transaction)
            .await
            .inspect_err(|err| {
                debug!(target: LOG_NET_API, %txid, err = %err, "Transaction rejected");
            })?;

        drop(tx);

        let _ = self
            .submission_sender
            .send(ConsensusItem::Transaction(transaction.clone()))
            .await
            .inspect_err(|err| {
                warn!(target: LOG_NET_API, %txid, err = %err, "Unable to submit the tx into consensus");
            });

        Ok(txid)
    }

    pub async fn await_transaction(&self, txid: TransactionId) -> ((), ReadTransaction) {
        debug!(target: LOG_NET_API, %txid, "Awaiting transaction acceptance");
        self.db
            .wait_key_check(&ACCEPTED_TRANSACTION, &txid, std::convert::identity)
            .await
    }

    pub async fn session_count(&self) -> u64 {
        get_finished_session_count_static(&self.db.begin_read().await).await
    }

    pub async fn federation_audit(&self) -> AuditSummary {
        // Modules read their own tables during `audit`; we open a write tx and
        // drop it without commit after building the audit view.
        let tx = self.db.begin_write().await;

        let mut audit = Audit::default();
        self.server.audit(&tx, &mut audit).await;

        AuditSummary::from_audit(&audit)
    }
}

impl ConsensusApi {
    pub async fn handle_api(
        &self,
        method: &str,
        req: ApiRequestErased,
    ) -> Result<Vec<u8>, ApiError> {
        match method {
            SUBMIT_TRANSACTION_ENDPOINT => handler!(submit_transaction, self, req).await,
            AWAIT_TRANSACTION_ENDPOINT => handler!(await_transaction, self, req).await,
            CLIENT_CONFIG_ENDPOINT => handler!(client_config, self, req).await,
            LIVENESS_ENDPOINT => handler!(liveness, self, req).await,
            other => Err(ApiError::not_found(other.to_string())),
        }
    }
}
