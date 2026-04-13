//! Implements the client API through which users interact with the federation
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use bitcoin::hashes::sha256;
use fedimint_core::config::{ClientConfig, JsonClientConfig, META_FEDERATION_NAME_KEY};
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::db::{
    Committable, Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped,
};
use fedimint_core::endpoint_constants::{
    AWAIT_SESSION_OUTCOME_ENDPOINT, AWAIT_SIGNED_SESSION_OUTCOME_ENDPOINT,
    AWAIT_TRANSACTION_ENDPOINT, CHAIN_ID_ENDPOINT, CLIENT_CONFIG_ENDPOINT,
    CLIENT_CONFIG_JSON_ENDPOINT, CONSENSUS_ORD_LATENCY_ENDPOINT, FEDERATION_ID_ENDPOINT,
    FEDIMINTD_VERSION_ENDPOINT, INVITE_CODE_ENDPOINT, P2P_CONNECTION_STATUS_ENDPOINT,
    SERVER_CONFIG_CONSENSUS_HASH_ENDPOINT, SESSION_COUNT_ENDPOINT, SESSION_STATUS_ENDPOINT,
    SESSION_STATUS_V2_ENDPOINT, SETUP_STATUS_ENDPOINT, SUBMIT_TRANSACTION_ENDPOINT,
    VERSION_ENDPOINT,
};
use fedimint_core::epoch::ConsensusItem;
use fedimint_core::module::audit::{Audit, AuditSummary};
use fedimint_core::module::{
    ApiAuth, ApiEndpoint, ApiEndpointContext, ApiError, ApiRequestErased, ApiResult, ApiVersion,
    SerdeModuleEncoding, SerdeModuleEncodingBase64, SupportedApiVersionsSummary, api_endpoint,
};
use fedimint_core::net::auth::GuardianAuthToken;
use fedimint_core::session_outcome::{
    SessionOutcome, SessionStatus, SessionStatusV2, SignedSessionOutcome,
};
use fedimint_core::task::TaskGroup;
use fedimint_core::transaction::{
    SerdeTransaction, Transaction, TransactionError, TransactionSubmissionOutcome,
};
use fedimint_core::util::{FmtCompact, SafeUrl};
use fedimint_core::{ChainId, PeerId, TransactionId};
use fedimint_logging::LOG_NET_API;
use fedimint_server_core::bitcoin_rpc::ServerBitcoinRpcMonitor;
use fedimint_server_core::dashboard_ui::{
    GuardianConfigBackup, IDashboardApi, P2PConnectionStatus, ServerBitcoinRpcStatus, SetupStatus,
};
use fedimint_server_core::{DynServerModule, ServerModuleRegistry, ServerModuleRegistryExt};
use futures::StreamExt;
use tokio::sync::watch::{self, Receiver, Sender};
use tracing::{debug, warn};

use crate::config::io::{CONSENSUS_CONFIG, JSON_EXT, LOCAL_CONFIG, PRIVATE_CONFIG};
use crate::config::{ServerConfig, legacy_consensus_config_hash};
use crate::consensus::db::{AcceptedItemPrefix, AcceptedTransactionKey, SignedSessionOutcomeKey};
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
    pub supported_api_versions: SupportedApiVersionsSummary,
    pub auth: ApiAuth,
    pub code_version_str: String,
    pub task_group: TaskGroup,
}

impl ConsensusApi {
    pub fn api_versions_summary(&self) -> &SupportedApiVersionsSummary {
        &self.supported_api_versions
    }

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

        // Create read-only DB tx so that the read state is consistent
        let mut dbtx = self.db.begin_transaction_nc().await;
        // we already processed the transaction before
        if dbtx
            .get_value(&AcceptedTransactionKey(txid))
            .await
            .is_some()
        {
            debug!(target: LOG_NET_API, %txid, "Transaction already accepted");
            return Ok(txid);
        }

        // We ignore any writes, as we only verify if the transaction is valid here
        dbtx.ignore_uncommitted();

        process_transaction_with_dbtx(self.modules.clone(), &mut dbtx, &transaction)
        .await
        .inspect_err(|err| {
            debug!(target: LOG_NET_API, %txid, err = %err.fmt_compact(), "Transaction rejected");
        })?;

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
    ) -> (Vec<ModuleInstanceId>, DatabaseTransaction<'_, Committable>) {
        debug!(target: LOG_NET_API, %txid, "Awaiting transaction acceptance");
        self.db
            .wait_key_check(&AcceptedTransactionKey(txid), std::convert::identity)
            .await
    }

    pub async fn session_count(&self) -> u64 {
        get_finished_session_count_static(&mut self.db.begin_transaction_nc().await).await
    }

    pub async fn await_signed_session_outcome(&self, index: u64) -> SignedSessionOutcome {
        self.db
            .wait_key_check(&SignedSessionOutcomeKey(index), std::convert::identity)
            .await
            .0
    }

    pub async fn session_status(&self, session_index: u64) -> SessionStatusV2 {
        let mut dbtx = self.db.begin_transaction_nc().await;

        match session_index.cmp(&get_finished_session_count_static(&mut dbtx).await) {
            Ordering::Greater => SessionStatusV2::Initial,
            Ordering::Equal => SessionStatusV2::Pending(
                dbtx.find_by_prefix(&AcceptedItemPrefix)
                    .await
                    .map(|entry| entry.1)
                    .collect()
                    .await,
            ),
            Ordering::Less => SessionStatusV2::Complete(
                dbtx.get_value(&SignedSessionOutcomeKey(session_index))
                    .await
                    .expect("There are no gaps in session outcomes"),
            ),
        }
    }

    async fn get_federation_audit(&self) -> ApiResult<AuditSummary> {
        let mut dbtx = self.db.begin_transaction_nc().await;
        // Writes are related to compacting audit keys, which we can safely ignore
        // within an API request since the compaction will happen when constructing an
        // audit in the consensus server
        dbtx.ignore_uncommitted();

        let mut audit = Audit::default();
        let mut module_instance_id_to_kind: HashMap<ModuleInstanceId, String> = HashMap::new();
        for (module_instance_id, kind, module) in self.modules.iter_modules() {
            module_instance_id_to_kind.insert(module_instance_id, kind.as_str().to_string());
            module
                .audit(
                    &mut dbtx.to_ref_with_prefix_module_id(module_instance_id).0,
                    &mut audit,
                    module_instance_id,
                )
                .await;
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

    /// Returns the tagged fedimintd version currently running
    fn fedimintd_version(&self) -> String {
        self.code_version_str.clone()
    }
}

#[async_trait]
impl HasApiContext<ConsensusApi> for ConsensusApi {
    async fn context(
        &self,
        request: &ApiRequestErased,
        id: Option<ModuleInstanceId>,
    ) -> (&ConsensusApi, ApiEndpointContext) {
        let mut db = self.db.clone();
        if let Some(id) = id {
            db = self.db.with_prefix_module_id(id).0;
        }
        (
            self,
            ApiEndpointContext::new(
                db,
                request
                    .auth
                    .as_ref()
                    .is_some_and(|a| self.auth.verify(a.as_str())),
                request.auth.clone(),
            ),
        )
    }
}

#[async_trait]
impl HasApiContext<DynServerModule> for ConsensusApi {
    async fn context(
        &self,
        request: &ApiRequestErased,
        id: Option<ModuleInstanceId>,
    ) -> (&DynServerModule, ApiEndpointContext) {
        let (_, context): (&ConsensusApi, _) = self.context(request, id).await;
        (
            self.modules.get_expect(id.expect("required module id")),
            context,
        )
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
        self.session_count().await
    }

    async fn get_session_status(&self, session_idx: u64) -> SessionStatusV2 {
        self.session_status(session_idx).await
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
            VERSION_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> SupportedApiVersionsSummary {
                Ok(fedimint.api_versions_summary().to_owned())
            }
        },
        api_endpoint! {
            SUBMIT_TRANSACTION_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, transaction: SerdeTransaction| -> SerdeModuleEncoding<TransactionSubmissionOutcome> {
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
            async |fedimint: &ConsensusApi, _context, tx_hash: TransactionId| -> TransactionId {
                fedimint.await_transaction(tx_hash).await;

                Ok(tx_hash)
            }
        },
        api_endpoint! {
            INVITE_CODE_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context,  _v: ()| -> String {
                Ok(fedimint.cfg.get_invite_code(fedimint.get_active_api_secret()).to_string())
            }
        },
        api_endpoint! {
            FEDERATION_ID_ENDPOINT,
            ApiVersion::new(0, 2),
            async |fedimint: &ConsensusApi, _context,  _v: ()| -> String {
                Ok(fedimint.cfg.calculate_federation_id().to_string())
            }
        },
        api_endpoint! {
            CLIENT_CONFIG_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> ClientConfig {
                Ok(fedimint.client_cfg.clone())
            }
        },
        // Helper endpoint for Admin UI that can't parse consensus encoding
        api_endpoint! {
            CLIENT_CONFIG_JSON_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> JsonClientConfig {
                Ok(fedimint.client_cfg.to_json())
            }
        },
        api_endpoint! {
            SERVER_CONFIG_CONSENSUS_HASH_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> sha256::Hash {
                Ok(legacy_consensus_config_hash(&fedimint.cfg.consensus))
            }
        },
        api_endpoint! {
            SETUP_STATUS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |_f: &ConsensusApi, _c, _v: ()| -> SetupStatus {
                Ok(SetupStatus::ConsensusIsRunning)
            }
        },
        api_endpoint! {
            CONSENSUS_ORD_LATENCY_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _c, _v: ()| -> Option<Duration> {
                Ok(*fedimint.ord_latency_receiver.borrow())
            }
        },
        api_endpoint! {
            P2P_CONNECTION_STATUS_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _c, _v: ()| -> BTreeMap<PeerId, Option<P2PConnectionStatus>> {
                Ok(fedimint.p2p_status_receivers
                    .iter()
                    .map(|(peer, receiver)| (*peer, receiver.borrow().clone()))
                    .collect())
            }
        },
        api_endpoint! {
            SESSION_COUNT_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> u64 {
                Ok(fedimint.session_count().await)
            }
        },
        api_endpoint! {
            AWAIT_SESSION_OUTCOME_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, index: u64| -> SerdeModuleEncoding<SessionOutcome> {
                Ok((&fedimint.await_signed_session_outcome(index).await.session_outcome).into())
            }
        },
        api_endpoint! {
            AWAIT_SIGNED_SESSION_OUTCOME_ENDPOINT,
            ApiVersion::new(0, 0),
            async |fedimint: &ConsensusApi, _context, index: u64| -> SerdeModuleEncoding<SignedSessionOutcome> {
                Ok((&fedimint.await_signed_session_outcome(index).await).into())
            }
        },
        api_endpoint! {
            SESSION_STATUS_ENDPOINT,
            ApiVersion::new(0, 1),
            async |fedimint: &ConsensusApi, _context, index: u64| -> SerdeModuleEncoding<SessionStatus> {
                Ok((&SessionStatus::from(fedimint.session_status(index).await)).into())
            }
        },
        api_endpoint! {
            SESSION_STATUS_V2_ENDPOINT,
            ApiVersion::new(0, 5),
            async |fedimint: &ConsensusApi, _context, index: u64| -> SerdeModuleEncodingBase64<SessionStatusV2> {
                Ok((&fedimint.session_status(index).await).into())
            }
        },
        api_endpoint! {
            FEDIMINTD_VERSION_ENDPOINT,
            ApiVersion::new(0, 4),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> String {
                Ok(fedimint.fedimintd_version())
            }
        },
        api_endpoint! {
            CHAIN_ID_ENDPOINT,
            ApiVersion::new(0, 9),
            async |fedimint: &ConsensusApi, _context, _v: ()| -> ChainId {
                fedimint
                    .bitcoin_rpc_connection
                    .get_chain_id()
                    .await
                    .map_err(|e| ApiError::server_error(e.to_string()))
            }
        },
    ]
}
