//! Implements the client API through which users interact with the federation

use std::collections::BTreeMap;

use anyhow::Result;
use picomint_bitcoin_rpc::BitcoinRpcMonitor;
use picomint_core::config::ConsensusConfig;
use picomint_core::endpoint_constants::{
    CLIENT_CONFIG_ENDPOINT, LIVENESS_ENDPOINT, SUBMIT_TRANSACTION_ENDPOINT,
};
use picomint_core::module::audit::{Audit, AuditSummary};
use picomint_core::module::{ApiError, ApiRequestErased};
use picomint_core::transaction::{ConsensusItem, Transaction, TransactionError};

use crate::consensus::rpc;
use crate::handler;
use picomint_core::PeerId;
use picomint_core::task::TaskGroup;
use picomint_logging::LOG_NET_API;
use picomint_redb::Database;
use tokio::sync::watch::{Receiver, Sender};
use tracing::warn;

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
    pub p2p_status_receivers: P2PStatusReceivers,
    pub ci_status_receivers: BTreeMap<PeerId, Receiver<Option<u64>>>,
    pub bitcoin_rpc_connection: BitcoinRpcMonitor,
    pub task_group: TaskGroup,
}

impl ConsensusApi {
    /// Submit a transaction and long-poll until it is either accepted by
    /// consensus or becomes invalid. On each commit we re-check: if the
    /// transaction is now in `ACCEPTED_TRANSACTION` return `Ok`; if it fails
    /// revalidation return `Err`; otherwise wait for the next commit.
    ///
    /// Relies on the invariant that module state is monotonic in the
    /// "time makes a tx valid" direction — a valid tx can only become invalid
    /// as the result of another tx being accepted (which fires the commit
    /// notify via [`Database::wait_commit`]).
    pub async fn submit_transaction(
        &self,
        transaction: Transaction,
    ) -> Result<(), TransactionError> {
        let tx = self.db.begin_write();

        if tx
            .get(&ACCEPTED_TRANSACTION, &transaction.tx_hash())
            .is_some()
        {
            return Ok(());
        }

        process_transaction_with_server(&self.server, &tx, &transaction).await?;

        drop(tx);

        if self
            .submission_sender
            .send(ConsensusItem::Transaction(transaction.clone()))
            .await
            .is_err()
        {
            warn!(target: LOG_NET_API, "Unable to submit the tx into consensus");
        }

        loop {
            let commit = self.db.wait_commit();

            let tx = self.db.begin_write();

            if tx
                .get(&ACCEPTED_TRANSACTION, &transaction.tx_hash())
                .is_some()
            {
                return Ok(());
            }

            process_transaction_with_server(&self.server, &tx, &transaction).await?;

            drop(tx);

            commit.await;
        }
    }

    pub async fn session_count(&self) -> u64 {
        get_finished_session_count_static(&self.db.begin_read()).await
    }

    pub async fn federation_audit(&self) -> AuditSummary {
        // Modules read their own tables during `audit`; we open a write tx and
        // drop it without commit after building the audit view.
        let tx = self.db.begin_write();

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
            CLIENT_CONFIG_ENDPOINT => handler!(client_config, self, req).await,
            LIVENESS_ENDPOINT => handler!(liveness, self, req).await,
            other => Err(ApiError::not_found(other.to_string())),
        }
    }
}
