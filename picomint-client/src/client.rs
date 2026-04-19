use std::any::{Any, TypeId};
use std::collections::BTreeMap;
use std::fmt::{self, Formatter};
use std::sync::Arc;

use crate::Endpoint;
use crate::api::{ApiScope, FederationApi};
use crate::gw::{GatewayClientModule, IGatewayClient};
use crate::ln::LightningClientModule;
use crate::mint::MintClientModule;
use crate::secret::{DeriveableSecretClientExt as _, get_default_client_secret};
use crate::wallet::WalletClientModule;
use crate::{ClientModuleInstance, TxAcceptEvent, TxRejectEvent, download_from_invite_code};
use anyhow::bail;
use futures::{Stream, StreamExt as _};
use picomint_core::PeerId;
use picomint_core::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::core::{ModuleKind, OperationId};
use picomint_core::invite_code::InviteCode;
use picomint_core::task::TaskGroup;
use picomint_core::util::BoxStream;
use picomint_core::{Amount, TransactionId};
use picomint_derive_secret::DerivableSecret;
use picomint_eventlog::{EventLogId, PersistedLogEntry};
use picomint_logging::LOG_CLIENT;
use picomint_redb::Database;
use tracing::debug;

use crate::db::CLIENT_CONFIG;

pub(crate) mod handle;

/// Pre-derivation step for the client root secret. Internally the client
/// hashes in the federation id so the same input secret is safely reused
/// across federations.
#[derive(Clone)]
pub enum RootSecret {
    /// Derive an extra round of federation-id into the secret. Applications
    /// MUST NOT do that derivation themselves.
    StandardDoubleDerive(DerivableSecret),
}

impl RootSecret {
    fn to_inner(&self, federation_id: FederationId) -> DerivableSecret {
        match self {
            RootSecret::StandardDoubleDerive(s) => get_default_client_secret(s, &federation_id),
        }
    }
}

/// LN-flavor selection used by the four constructors below.
enum LnChoice {
    Regular,
    Gateway(Arc<dyn IGatewayClient>),
}

/// Lightning-module flavor mounted on a client. Regular federation clients
/// use `Regular`, while the gateway daemon mounts `Gateway`. The two flavors
/// are mutually exclusive at the same federation instance.
pub enum LnFlavor {
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
/// Under the hood it is starting and managing service tasks, state machines,
/// database and other resources required.
///
/// This type is shared externally and internally, and
/// [`crate::ClientHandle`] is responsible for external lifecycle management
/// and resource freeing of the [`Client`].
pub struct Client {
    config: tokio::sync::RwLock<ConsensusConfig>,
    connectors: Endpoint,
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
    pub async fn join(
        connectors: Endpoint,
        db: Database,
        root_secret: RootSecret,
        invite: &InviteCode,
    ) -> anyhow::Result<handle::ClientHandle> {
        let config = download_from_invite_code(&connectors, invite).await?;
        Self::init_db(&db, &config).await?;
        Self::build(connectors, db, root_secret, config, LnChoice::Regular).await
    }

    /// Open an existing regular-lightning federation client from a database
    /// that was previously initialized via [`Client::join`].
    pub async fn open(
        connectors: Endpoint,
        db: Database,
        root_secret: RootSecret,
    ) -> anyhow::Result<handle::ClientHandle> {
        let config = Self::get_config_from_db(&db)
            .await
            .ok_or_else(|| anyhow::anyhow!("Client database not initialized"))?;
        Self::build(connectors, db, root_secret, config, LnChoice::Regular).await
    }

    /// Gateway-flavor counterpart of [`Client::join`]. Used by the gateway
    /// daemon, which mounts its own [`IGatewayClient`] in place of the
    /// regular lightning module.
    pub async fn join_gateway(
        connectors: Endpoint,
        db: Database,
        root_secret: RootSecret,
        invite: &InviteCode,
        gateway: Arc<dyn IGatewayClient>,
    ) -> anyhow::Result<handle::ClientHandle> {
        let config = download_from_invite_code(&connectors, invite).await?;
        Self::init_db(&db, &config).await?;
        Self::build(connectors, db, root_secret, config, LnChoice::Gateway(gateway)).await
    }

    /// Gateway-flavor counterpart of [`Client::open`].
    pub async fn open_gateway(
        connectors: Endpoint,
        db: Database,
        root_secret: RootSecret,
        gateway: Arc<dyn IGatewayClient>,
    ) -> anyhow::Result<handle::ClientHandle> {
        let config = Self::get_config_from_db(&db)
            .await
            .ok_or_else(|| anyhow::anyhow!("Client database not initialized"))?;
        Self::build(connectors, db, root_secret, config, LnChoice::Gateway(gateway)).await
    }

    async fn init_db(db: &Database, config: &ConsensusConfig) -> anyhow::Result<()> {
        if Self::is_initialized(db).await {
            bail!("Client database already initialized")
        }
        debug!(target: LOG_CLIENT, "Initializing client database");
        let dbtx = db.begin_write().await;
        dbtx.as_ref().insert(&CLIENT_CONFIG, &(), config);
        dbtx.commit().await;
        Ok(())
    }

    async fn build(
        connectors: Endpoint,
        db: Database,
        pre_root_secret: RootSecret,
        config: ConsensusConfig,
        ln_choice: LnChoice,
    ) -> anyhow::Result<handle::ClientHandle> {
        debug!(
            target: LOG_CLIENT,
            version = %env!("CARGO_PKG_VERSION"),
            "Building picomint client",
        );
        let fed_id = config.calculate_federation_id();
        let pre_root_secret = pre_root_secret.to_inner(fed_id);
        let root_secret = pre_root_secret.federation_key(&fed_id);

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
            db.isolate("mint".to_string()),
            config.clone(),
            fed_id,
        );
        let mint = Arc::new(
            MintClientModule::new(
                fed_id,
                config.mint.clone(),
                mint_context,
                db.clone(),
                &root_secret.derive_module_secret(ModuleKind::Mint),
                &task_group,
            )
            .await?,
        );

        let wallet_context = crate::module::ClientContext::new(
            ModuleKind::Wallet,
            api.clone(),
            ApiScope::Wallet,
            db.clone(),
            db.isolate("wallet".to_string()),
            config.clone(),
            fed_id,
        );
        let wallet = Arc::new(
            WalletClientModule::new(
                config.wallet.clone(),
                wallet_context,
                mint.clone(),
                &root_secret.derive_module_secret(ModuleKind::Wallet),
                &task_group,
            )
            .await?,
        );

        let ln_secret = root_secret.derive_module_secret(ModuleKind::Ln);
        let ln = match ln_choice {
            LnChoice::Regular => {
                let ln_context = crate::module::ClientContext::<LightningClientModule>::new(
                    ModuleKind::Ln,
                    api.clone(),
                    ApiScope::Ln,
                    db.clone(),
                    db.isolate("ln".to_string()),
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
                    db.isolate("ln".to_string()),
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

        let client_inner = Arc::new(Client {
            config: tokio::sync::RwLock::new(config.clone()),
            db,
            connectors,
            federation_id: fed_id,
            federation_config_meta: config.meta,
            mint,
            wallet,
            ln,
            api,
            task_group: task_group.clone(),
        });

        Ok(handle::ClientHandle::new(client_inner))
    }

    pub fn api(&self) -> &FederationApi {
        &self.api
    }

    pub fn api_clone(&self) -> FederationApi {
        self.api.clone()
    }

    /// Returns a stream that emits the current connection status of all peers
    /// whenever any peer's status changes. Emits initial state immediately.
    pub fn connection_status_stream(&self) -> impl Stream<Item = BTreeMap<PeerId, bool>> {
        self.api.connection_status_stream()
    }

    /// Get the [`TaskGroup`] that is tied to Client's lifetime.
    pub fn task_group(&self) -> &TaskGroup {
        &self.task_group
    }

    pub async fn get_config_from_db(db: &Database) -> Option<ConsensusConfig> {
        db.begin_read().await.as_ref().get(&CLIENT_CONFIG, &())
    }

    pub async fn is_initialized(db: &Database) -> bool {
        Self::get_config_from_db(db).await.is_some()
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

    pub async fn await_tx_accepted(
        &self,
        operation_id: OperationId,
        query_txid: TransactionId,
    ) -> Result<(), String> {
        let mut stream = self.subscribe_operation_events(operation_id);
        while let Some(entry) = stream.next().await {
            if let Some(ev) = entry.to_event::<TxAcceptEvent>()
                && ev.txid == query_txid
            {
                return Ok(());
            }
            if let Some(ev) = entry.to_event::<TxRejectEvent>()
                && ev.txid == query_txid
            {
                return Err(ev.error);
            }
        }
        unreachable!("subscribe_operation_events only ends at client shutdown")
    }

    /// Returns a typed module client instance by type. Uses `TypeId` dispatch
    /// over the fixed module set (`MintClientModule` / `WalletClientModule` /
    /// `LightningClientModule` / `GatewayClientModule`).
    pub fn get_first_module<M: Any + Send + Sync + 'static>(
        &'_ self,
    ) -> anyhow::Result<ClientModuleInstance<'_, M>> {
        let tid = TypeId::of::<M>();
        let (module_any, kind, scope): (&(dyn Any + Send + Sync), ModuleKind, ApiScope) =
            if tid == TypeId::of::<MintClientModule>() {
                (&*self.mint, ModuleKind::Mint, ApiScope::Mint)
            } else if tid == TypeId::of::<WalletClientModule>() {
                (&*self.wallet, ModuleKind::Wallet, ApiScope::Wallet)
            } else if tid == TypeId::of::<LightningClientModule>() {
                match &self.ln {
                    LnFlavor::Regular(m) => (&**m, ModuleKind::Ln, ApiScope::Ln),
                    LnFlavor::Gateway(_) => {
                        bail!("LightningClientModule is not mounted on this client")
                    }
                }
            } else if tid == TypeId::of::<GatewayClientModule>() {
                match &self.ln {
                    LnFlavor::Gateway(m) => (&**m, ModuleKind::Ln, ApiScope::Ln),
                    LnFlavor::Regular(_) => {
                        bail!("GatewayClientModule is not mounted on this client")
                    }
                }
            } else {
                bail!("Unknown client module type");
            };

        let module: &M = module_any
            .downcast_ref::<M>()
            .expect("TypeId of M was just matched");
        let db = self.db().isolate(match kind {
            ModuleKind::Mint => "mint".to_string(),
            ModuleKind::Wallet => "wallet".to_string(),
            ModuleKind::Ln => "ln".to_string(),
        });
        Ok(ClientModuleInstance {
            db,
            api: self.api().with_scope(scope),
            module,
        })
    }

    pub fn db(&self) -> &Database {
        &self.db
    }

    pub fn endpoints(&self) -> &Endpoint {
        &self.connectors
    }

    pub async fn get_balance(&self) -> anyhow::Result<Amount> {
        let dbtx = self.db().begin_write().await;
        Ok(self
            .mint
            .get_balance(&dbtx.as_ref().isolate("mint".to_string()))
            .await)
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
                let balance = mint
                    .get_balance(
                        &dbtx.as_ref().isolate("mint".to_string()),
                    )
                    .await;

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
