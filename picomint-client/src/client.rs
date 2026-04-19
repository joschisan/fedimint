use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use crate::Endpoint;
use crate::api::{ApiScope, FederationApi};
use crate::gw::{GatewayClientModule, IGatewayClient};
use crate::ln::LightningClientModule;
use crate::mint::MintClientModule;
use crate::secret::{Mnemonic, derive_module_secret, derive_root_secret};
use crate::wallet::WalletClientModule;
use futures::Stream;
use picomint_core::Amount;
use picomint_core::PeerId;
use picomint_core::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::core::{ModuleKind, OperationId};
use picomint_core::invite_code::InviteCode;
use picomint_core::task::TaskGroup;
use picomint_core::util::BoxStream;
use picomint_eventlog::{EventLogId, PersistedLogEntry};
use picomint_logging::LOG_CLIENT;
use picomint_redb::Database;
use tracing::debug;

/// LN-flavor selection used by the two constructors below.
enum LnChoice {
    Regular,
    Gateway(Arc<dyn IGatewayClient>),
}

/// Lightning-module flavor mounted on a client. Regular federation clients
/// use `Regular`, while the gateway daemon mounts `Gateway`. The two flavors
/// are mutually exclusive at the same federation instance.
pub(crate) enum LnFlavor {
    Regular(Arc<LightningClientModule>),
    Gateway(Arc<GatewayClientModule>),
}

/// Main client type
///
/// A handle and API to interacting with a single federation. End user
/// applications that want to support interacting with multiple federations at
/// the same time, will need to instantiate and manage multiple instances of
/// this struct.
///
/// Under the hood it owns service tasks, state machines, database, and other
/// resources. Dropping the last [`Arc<Client>`] cancels all spawned tasks
/// (best-effort, non-blocking); call [`Client::shutdown`] explicitly to wait
/// for them to finish.
pub struct Client {
    config: tokio::sync::RwLock<ConsensusConfig>,
    db: Database,
    federation_id: FederationId,
    federation_config_meta: BTreeMap<String, String>,
    pub(crate) mint: Arc<MintClientModule>,
    pub(crate) wallet: Arc<WalletClientModule>,
    pub(crate) ln: LnFlavor,
    pub(crate) api: FederationApi,
    task_group: TaskGroup,
}

impl Client {
    /// Join a federation for the first time using a regular lightning
    /// flavor. Downloads the federation config via the invite, persists it,
    /// and brings up the client.
    pub async fn new(
        connectors: Endpoint,
        db: Database,
        mnemonic: &Mnemonic,
        config: ConsensusConfig,
    ) -> anyhow::Result<Arc<Self>> {
        Self::build(connectors, db, mnemonic, config, LnChoice::Regular).await
    }

    /// Gateway-flavor counterpart of [`Client::new`]. Used by the gateway
    /// daemon, which mounts its own [`IGatewayClient`] in place of the
    /// regular lightning module.
    pub async fn new_gateway(
        connectors: Endpoint,
        db: Database,
        mnemonic: &Mnemonic,
        config: ConsensusConfig,
        gateway: Arc<dyn IGatewayClient>,
    ) -> anyhow::Result<Arc<Self>> {
        Self::build(connectors, db, mnemonic, config, LnChoice::Gateway(gateway)).await
    }

    async fn build(
        connectors: Endpoint,
        db: Database,
        mnemonic: &Mnemonic,
        config: ConsensusConfig,
        ln_choice: LnChoice,
    ) -> anyhow::Result<Arc<Self>> {
        debug!(
            target: LOG_CLIENT,
            version = %env!("CARGO_PKG_VERSION"),
            "Building picomint client",
        );
        let fed_id = config.calculate_federation_id();
        let root_secret = derive_root_secret(mnemonic, fed_id);

        let peer_node_ids: BTreeMap<PeerId, iroh_base::PublicKey> = config
            .iroh_endpoints
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.node_id))
            .collect();
        let api: FederationApi = FederationApi::new(connectors.clone(), peer_node_ids);

        let task_group = TaskGroup::new();

        let mint_context = crate::module::ClientContext::new(
            ModuleKind::Mint,
            api.clone(),
            ApiScope::Mint,
            db.clone(),
            config.clone(),
            fed_id,
        );
        let mint = Arc::new(
            MintClientModule::new(
                fed_id,
                config.mint.clone(),
                mint_context,
                &derive_module_secret(&root_secret, ModuleKind::Mint),
                &task_group,
            )
            .await?,
        );

        let wallet_context = crate::module::ClientContext::new(
            ModuleKind::Wallet,
            api.clone(),
            ApiScope::Wallet,
            db.clone(),
            config.clone(),
            fed_id,
        );
        let wallet = Arc::new(
            WalletClientModule::new(
                config.wallet.clone(),
                wallet_context,
                mint.clone(),
                &derive_module_secret(&root_secret, ModuleKind::Wallet),
                &task_group,
            )
            .await?,
        );

        let ln_secret = derive_module_secret(&root_secret, ModuleKind::Ln);
        let ln = match ln_choice {
            LnChoice::Regular => {
                let ln_context = crate::module::ClientContext::<LightningClientModule>::new(
                    ModuleKind::Ln,
                    api.clone(),
                    ApiScope::Ln,
                    db.clone(),
                    config.clone(),
                    fed_id,
                );
                LnFlavor::Regular(Arc::new(
                    LightningClientModule::new(
                        fed_id,
                        config.ln.clone(),
                        ln_context,
                        mint.clone(),
                        &ln_secret,
                        &task_group,
                    )
                    .await?,
                ))
            }
            LnChoice::Gateway(gateway) => {
                let gw_context = crate::module::ClientContext::<GatewayClientModule>::new(
                    ModuleKind::Ln,
                    api.clone(),
                    ApiScope::Ln,
                    db.clone(),
                    config.clone(),
                    fed_id,
                );
                LnFlavor::Gateway(Arc::new(
                    GatewayClientModule::new(
                        fed_id,
                        config.ln.clone(),
                        gw_context,
                        mint.clone(),
                        gateway,
                        &ln_secret,
                        &task_group,
                    )
                    .await?,
                ))
            }
        };

        Ok(Arc::new(Client {
            config: tokio::sync::RwLock::new(config.clone()),
            db,
            federation_id: fed_id,
            federation_config_meta: config.meta,
            mint,
            wallet,
            ln,
            api,
            task_group,
        }))
    }

    /// Cancel all spawned tasks and wait for them to finish. No timeout —
    /// blocks until every state machine driver and background task has
    /// observed cancellation and exited cleanly.
    pub async fn shutdown(&self) {
        let _ = self.task_group.clone().shutdown_join_all(None).await;
    }

    pub fn api(&self) -> &FederationApi {
        &self.api
    }

    /// Returns a stream that emits the current connection status of all peers
    /// whenever any peer's status changes. Emits initial state immediately.
    pub fn connection_status_stream(&self) -> impl Stream<Item = BTreeMap<PeerId, bool>> {
        self.api.connection_status_stream()
    }

    pub fn federation_id(&self) -> FederationId {
        self.federation_id
    }

    pub async fn config(&self) -> ConsensusConfig {
        self.config.read().await.clone()
    }

    /// Get metadata value from the federation config itself
    pub fn get_config_meta(&self, key: &str) -> Option<String> {
        self.federation_config_meta.get(key).cloned()
    }

    pub fn mint(&self) -> &MintClientModule {
        &self.mint
    }

    pub fn wallet(&self) -> &WalletClientModule {
        &self.wallet
    }

    /// Regular-flavor lightning module. Panics if this client mounts the
    /// gateway flavor instead.
    pub fn ln(&self) -> &LightningClientModule {
        match &self.ln {
            LnFlavor::Regular(m) => m,
            LnFlavor::Gateway(_) => panic!("LightningClientModule is not mounted on this client"),
        }
    }

    /// Gateway-flavor lightning module. Panics if this client mounts the
    /// regular flavor instead.
    pub fn gw(&self) -> &GatewayClientModule {
        match &self.ln {
            LnFlavor::Gateway(m) => m,
            LnFlavor::Regular(_) => panic!("GatewayClientModule is not mounted on this client"),
        }
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub async fn get_balance(&self) -> anyhow::Result<Amount> {
        let dbtx = self.db().begin_write().await;
        Ok(self.mint.get_balance(&dbtx.as_ref()).await)
    }

    /// Returns a stream that yields the current client balance every time it
    /// changes.
    pub async fn subscribe_balance_changes(&self) -> BoxStream<'static, Amount> {
        let notify = self.mint.balance_notify();
        let initial_balance = self.get_balance().await.expect("Primary is present");
        let mint = self.mint.clone();
        let db = self.db().clone();

        Box::pin(async_stream::stream! {
            yield initial_balance;
            let mut prev_balance = initial_balance;
            loop {
                let notified = notify.notified();
                let dbtx = db.begin_write().await;
                let balance = mint.get_balance(&dbtx.as_ref()).await;

                // Deduplicate in case modules cannot always tell if the balance actually changed
                if balance != prev_balance {
                    prev_balance = balance;
                    yield balance;
                }
                notified.await;
            }
        })
    }

    /// Returns a list of guardian iroh API node ids
    pub async fn get_peer_node_ids(&self) -> BTreeMap<PeerId, iroh_base::PublicKey> {
        self.config()
            .await
            .iroh_endpoints
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.node_id))
            .collect()
    }

    /// Create an invite code with the api endpoint of the given peer which can
    /// be used to download this client config
    pub async fn invite_code(&self, peer: PeerId) -> Option<InviteCode> {
        self.get_peer_node_ids()
            .await
            .into_iter()
            .find_map(|(peer_id, node_id)| (peer == peer_id).then_some(node_id))
            .map(|node_id| InviteCode::new(node_id, peer, self.federation_id()))
    }

    /// Returns the guardian public key set from the client config.
    pub async fn get_guardian_public_keys_blocking(
        &self,
    ) -> BTreeMap<PeerId, picomint_core::secp256k1::PublicKey> {
        self.config().await.broadcast_public_keys
    }

    pub async fn get_event_log(
        &self,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        let pos = pos.unwrap_or(EventLogId::LOG_START);
        let end = pos.saturating_add(limit);
        self.db
            .begin_read()
            .await
            .as_ref()
            .with_native_table(&picomint_eventlog::EVENT_LOG, |t| {
                t.range(pos..end)
                    .expect("redb range failed")
                    .map(|r| {
                        let (k, v) = r.expect("redb range item failed");
                        picomint_eventlog::PersistedLogEntry::new(k.value(), v.value())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// Shared [`Notify`] that fires on every commit touching the event log.
    pub fn event_notify(&self) -> Arc<tokio::sync::Notify> {
        self.db.notify_for_table(&picomint_eventlog::EVENT_LOG)
    }

    /// Stream every event belonging to `operation_id`, starting from the
    /// beginning of the log (existing events first, then live ones).
    pub fn subscribe_operation_events(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, PersistedLogEntry> {
        Box::pin(picomint_eventlog::subscribe_operation_events(
            self.db.clone(),
            self.event_notify(),
            operation_id,
        ))
    }
}

// TODO: impl `Debug` for `Client` and derive here
impl fmt::Debug for Client {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "Client")
    }
}

/// Cancel-only on drop. Spawned tasks observe the cancellation token at
/// the next await and unwind. Callers wanting to wait for tasks to
/// complete should `client.shutdown().await` first.
impl Drop for Client {
    fn drop(&mut self) {
        self.task_group.shutdown();
    }
}
