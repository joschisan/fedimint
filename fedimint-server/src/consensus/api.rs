//! Implements the client API through which users interact with the federation
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use fedimint_core::config::{ClientConfig, META_FEDERATION_NAME_KEY};
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::db::v2::IReadDatabaseTransactionOpsTyped as _;
use fedimint_core::endpoint_constants::{
    AWAIT_TRANSACTION_ENDPOINT, CLIENT_CONFIG_ENDPOINT, LIVENESS_ENDPOINT,
    SUBMIT_TRANSACTION_ENDPOINT,
};
use fedimint_core::epoch::ConsensusItem;
use fedimint_core::module::audit::{Audit, AuditSummary};
use fedimint_core::module::{
    ApiAuth, ApiEndpoint, ApiError, ApiRequestErased, ApiResult, ApiVersion, SerdeModuleEncoding,
    api_endpoint,
};
use fedimint_core::net::auth::GuardianAuthToken;
use fedimint_core::session_outcome::SessionStatusV2;
use fedimint_core::task::TaskGroup;
use fedimint_core::transaction::{
    SerdeTransaction, Transaction, TransactionError, TransactionSubmissionOutcome,
};
use fedimint_core::util::{FmtCompact, SafeUrl};
use fedimint_core::{PeerId, TransactionId};
use fedimint_logging::LOG_NET_API;
use fedimint_redb::v2::{Database, ReadTransaction};
use fedimint_server_core::bitcoin_rpc::ServerBitcoinRpcMonitor;
use fedimint_server_core::dashboard_ui::{
    GuardianConfigBackup, IDashboardApi, P2PConnectionStatus, ServerBitcoinRpcStatus,
};
use fedimint_server_core::{DynServerModule, ServerModuleRegistry, ServerModuleRegistryExt};
use tokio::sync::watch::{self, Receiver, Sender};
use tracing::{debug, warn};

use crate::config::ServerConfig;
use crate::config::io::{CONSENSUS_CONFIG, JSON_EXT, LOCAL_CONFIG, PRIVATE_CONFIG};
use crate::consensus::db::{ACCEPTED_ITEM, ACCEPTED_TRANSACTION, SIGNED_SESSION_OUTCOME};
use crate::consensus::engine::get_finished_session_count_static;
use crate::consensus::transaction::process_transaction_with_dbtx;
use crate::net::HasApiContext;
use crate::net::p2p::P2PStatusReceivers;

#[derive(Clone)]
pub struct ConsensusApi {
    /// Our server configuration
    pub cfg: ServerConfig,
    /// Database for serving the API
    pub db: Database,
    /// Modules registered with the federation
    pub modules: ServerModuleRegistry,
    /// Cached client config
    pub client_cfg: ClientConfig,
    pub force_api_secret: Option<String>,
    /// For sending API events to consensus such as transactions
    pub submission_sender: async_channel::Sender<ConsensusItem>,
    pub shutdown_receiver: Receiver<Option<u64>>,
    pub shutdown_sender: Sender<Option<u64>>,
    pub ord_latency_receiver: watch::Receiver<Option<Duration>>,
    pub p2p_status_receivers: P2PStatusReceivers,
    pub ci_status_receivers: BTreeMap<PeerId, Receiver<Option<u64>>>,
    pub bitcoin_rpc_connection: ServerBitcoinRpcMonitor,
    pub auth: ApiAuth,
    pub code_version_str: String,
    pub task_group: TaskGroup,
}

impl ConsensusApi {
    pub fn get_active_api_secret(&self) -> Option<String> {
        // TODO: In the future, we might want to fetch it from the DB, so it's possible
        // to customize from the UX
        self.force_api_secret.clone()
    }

    // we want to return an error if and only if the submitted transaction is
    // invalid and will be rejected if we were to submit it to consensus
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

        process_transaction_with_dbtx(self.modules.clone(), &tx, &transaction)
        .await
        .inspect_err(|err| {
            debug!(target: LOG_NET_API, %txid, err = %err.fmt_compact(), "Transaction rejected");
        })?;

        drop(tx);

        let _ = self
            .submission_sender
            .send(ConsensusItem::Transaction(transaction.clone()))
            .await
            .inspect_err(|err| {
                warn!(target: LOG_NET_API, %txid, err = %err.fmt_compact(), "Unable to submit the tx into consensus");
            });

        Ok(txid)
    }

    pub async fn await_transaction(
        &self,
        txid: TransactionId,
    ) -> (Vec<ModuleInstanceId>, ReadTransaction) {
        debug!(target: LOG_NET_API, %txid, "Awaiting transaction acceptance");
        self.db
            .wait_key_check(&ACCEPTED_TRANSACTION, &txid, std::convert::identity)
            .await
    }

    async fn session_count_internal(&self) -> u64 {
        get_finished_session_count_static(&self.db.begin_read().await).await
    }

    async fn session_status_internal(&self, session_index: u64) -> SessionStatusV2 {
        let tx = self.db.begin_read().await;

        match session_index.cmp(&get_finished_session_count_static(&tx).await) {
            Ordering::Greater => SessionStatusV2::Initial,
            Ordering::Equal => SessionStatusV2::Pending(
                tx.iter(&ACCEPTED_ITEM)
                    .into_iter()
                    .map(|(_, v)| v)
                    .collect(),
            ),
            Ordering::Less => SessionStatusV2::Complete(
                tx.get(&SIGNED_SESSION_OUTCOME, &session_index)
                    .expect("There are no gaps in session outcomes"),
            ),
        }
    }

    async fn get_federation_audit(&self) -> ApiResult<AuditSummary> {
        // Modules read their own tables during `audit`; we open a write tx and
        // drop it without commit after building the audit view. Matches the
        // reference `migrate_to_redb_2` branch.
        let tx = self.db.begin_write().await;

        let mut audit = Audit::default();
        let mut module_instance_id_to_kind: HashMap<ModuleInstanceId, String> = HashMap::new();
        for (module_instance_id, kind, module) in self.modules.iter_modules() {
            module_instance_id_to_kind.insert(module_instance_id, kind.as_str().to_string());
            let view = tx.isolate(format!("module-{module_instance_id}"));
            module.audit(&view, &mut audit, module_instance_id).await;
        }
        Ok(AuditSummary::from_audit(
            &audit,
            &module_instance_id_to_kind,
        ))
    }

    /// Uses the in-memory config to write a config backup tar archive that
    /// guardians can download. Private keys are stored as plaintext JSON.
    /// Operators should rely on disk encryption for at-rest protection.
    fn get_guardian_config_backup(&self, _auth: &GuardianAuthToken) -> GuardianConfigBackup {
        let mut tar_archive_builder = tar::Builder::new(Vec::new());

        let mut append = |name: &Path, data: &[u8]| {
            let mut header = tar::Header::new_gnu();
            header.set_path(name).expect("Error setting path");
            header.set_size(data.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();
            tar_archive_builder
                .append(&header, data)
                .expect("Error adding data to tar archive");
        };

        append(
            &PathBuf::from(LOCAL_CONFIG).with_extension(JSON_EXT),
            &serde_json::to_vec(&self.cfg.local).expect("Error encoding local config"),
        );

        append(
            &PathBuf::from(CONSENSUS_CONFIG).with_extension(JSON_EXT),
            &serde_json::to_vec(&self.cfg.consensus).expect("Error encoding consensus config"),
        );

        append(
            &PathBuf::from(PRIVATE_CONFIG).with_extension(JSON_EXT),
            &serde_json::to_vec(&self.cfg.private).expect("Error encoding private config"),
        );

        let tar_archive_bytes = tar_archive_builder
            .into_inner()
            .expect("Error building tar archive");

        GuardianConfigBackup { tar_archive_bytes }
    }
}

#[async_trait]
impl HasApiContext<ConsensusApi> for ConsensusApi {
    async fn context(
        &self,
        _request: &ApiRequestErased,
        _id: Option<ModuleInstanceId>,
    ) -> &ConsensusApi {
        self
    }
}

#[async_trait]
impl HasApiContext<DynServerModule> for ConsensusApi {
    async fn context(
        &self,
        _request: &ApiRequestErased,
        id: Option<ModuleInstanceId>,
    ) -> &DynServerModule {
        self.modules.get_expect(id.expect("required module id"))
    }
}

#[async_trait]
impl IDashboardApi for ConsensusApi {
    async fn auth(&self) -> ApiAuth {
        self.auth.clone()
    }

    async fn guardian_id(&self) -> PeerId {
        self.cfg.local.identity
    }

    async fn guardian_names(&self) -> BTreeMap<PeerId, String> {
        self.cfg
            .consensus
            .api_endpoints()
            .iter()
            .map(|(peer_id, endpoint)| (*peer_id, endpoint.name.clone()))
            .collect()
    }

    async fn federation_name(&self) -> String {
        self.cfg
            .consensus
            .meta
            .get(META_FEDERATION_NAME_KEY)
            .cloned()
            .expect("Federation name must be set")
    }

    async fn session_count(&self) -> u64 {
        self.session_count_internal().await
    }

    async fn get_session_status(&self, session_idx: u64) -> SessionStatusV2 {
        self.session_status_internal(session_idx).await
    }

    async fn consensus_ord_latency(&self) -> Option<Duration> {
        *self.ord_latency_receiver.borrow()
    }

    async fn p2p_connection_status(&self) -> BTreeMap<PeerId, Option<P2PConnectionStatus>> {
        self.p2p_status_receivers
            .iter()
            .map(|(peer, receiver)| (*peer, receiver.borrow().clone()))
            .collect()
    }

    async fn federation_invite_code(&self) -> String {
        self.cfg
            .get_invite_code(self.get_active_api_secret())
            .to_string()
    }

    async fn federation_audit(&self) -> AuditSummary {
        self.get_federation_audit()
            .await
            .expect("Failed to get federation audit")
    }

    async fn bitcoin_rpc_url(&self) -> SafeUrl {
        self.bitcoin_rpc_connection.url()
    }

    async fn bitcoin_rpc_status(&self) -> Option<ServerBitcoinRpcStatus> {
        self.bitcoin_rpc_connection.status()
    }

    async fn download_guardian_config_backup(
        &self,
        guardian_auth: &GuardianAuthToken,
    ) -> GuardianConfigBackup {
        self.get_guardian_config_backup(guardian_auth)
    }

    fn get_module_by_kind(&self, kind: ModuleKind) -> Option<&DynServerModule> {
        self.modules
            .iter_modules()
            .find_map(|(_, module_kind, module)| {
                if *module_kind == kind {
                    Some(module)
                } else {
                    None
                }
            })
    }

    async fn fedimintd_version(&self) -> String {
        self.code_version_str.clone()
    }
}

pub fn server_endpoints() -> Vec<ApiEndpoint<ConsensusApi>> {
    vec![
        api_endpoint! {
            SUBMIT_TRANSACTION_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, transaction: SerdeTransaction| -> SerdeModuleEncoding<TransactionSubmissionOutcome> {
                let transaction = transaction
                    .try_into_inner(&fedimint.modules.decoder_registry())
                    .map_err(|e| ApiError::bad_request(e.to_string()))?;

                // we return an inner error if and only if the submitted transaction is
                // invalid and will be rejected if we were to submit it to consensus
                Ok((&TransactionSubmissionOutcome(fedimint.submit_transaction(transaction).await)).into())
            }
        },
        api_endpoint! {
            AWAIT_TRANSACTION_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, tx_hash: TransactionId| -> TransactionId {
                fedimint.await_transaction(tx_hash).await;

                Ok(tx_hash)
            }
        },
        api_endpoint! {
            CLIENT_CONFIG_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _v: ()| -> ClientConfig {
                Ok(fedimint.client_cfg.clone())
            }
        },
        api_endpoint! {
            LIVENESS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |_fedimint: &ConsensusApi, _v: ()| -> () {
                Ok(())
            }
        },
    ]
}
