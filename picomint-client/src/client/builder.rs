use std::collections::BTreeMap;
use std::sync::Arc;

use crate::api::{ApiScope, FederationApi};
use crate::executor::ModuleExecutor;
use crate::gw::{GatewayClientInit, IGatewayClient};
use crate::ln::LightningClientInit;
use crate::mint::MintClientInit;
use crate::module::LateClient;
use crate::secret::{DeriveableSecretClientExt as _, get_default_client_secret};
use crate::transaction::TxSubmissionSmContext;
use crate::wallet::WalletClientInit;
use crate::{Endpoint, download_from_invite_code};
use anyhow::bail;
use bitcoin::key::Secp256k1;
use picomint_core::PeerId;
use picomint_core::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::core::ModuleKind;
use picomint_core::invite_code::InviteCode;
use picomint_core::task::TaskGroup;
use picomint_derive_secret::DerivableSecret;
use picomint_logging::LOG_CLIENT;
use picomint_redb::Database;
use tracing::{debug, trace};

use super::handle::ClientHandle;
use super::{Client, LnFlavor};
use crate::db::CLIENT_CONFIG;

/// The type of root secret hashing
///
/// *Please read this documentation carefully if, especially if you're upgrading
/// downstream Picomint client application.*
///
/// Internally, client will always hash-in federation id
/// to the root secret provided to the [`ClientBuilder`],
/// to ensure a different actual root secret is used for ever federation.
/// This makes reusing a single root secret for different federations
/// in a multi-federation client, perfectly fine, and frees the client
/// from worrying about `FederationId`.
///
/// However, in the past Picomint applications (including `picomint-cli`)
/// were doing the hashing-in of `FederationId` outside of `picomint-client` as
/// well, which lead to effectively doing it twice, and pushed downloading of
/// the client config on join to application code, a sub-optimal API, especially
/// after joining federation needed to handle even more functionality.
///
/// To keep the interoperability of the seed phrases this double-derivation
/// is preserved, due to other architectural reason, `picomint-client`
/// will now do the outer-derivation internally as well.
#[derive(Clone)]
pub enum RootSecret {
    /// Derive an extra round of federation-id to the secret, like
    /// Picomint applications were doing manually in the past.
    ///
    /// **Note**: Applications MUST NOT do the derivation themselves anymore.
    StandardDoubleDerive(DerivableSecret),
}

impl RootSecret {
    fn to_inner(&self, federation_id: FederationId) -> DerivableSecret {
        match self {
            RootSecret::StandardDoubleDerive(derivable_secret) => {
                get_default_client_secret(derivable_secret, &federation_id)
            }
        }
    }
}

/// Choice of LN-module client flavor mounted on the built [`Client`].
///
/// Regular applications get `Regular`; the gateway daemon supplies `Gateway`
/// via [`ClientBuilder::with_gateway_ln`]. The two are mutually exclusive
/// because the federation-side ln module at instance id `1` accepts only
/// one client-side wrapper.
pub enum LnInit {
    Regular(LightningClientInit),
    Gateway(GatewayClientInit),
}

/// Used to configure, assemble and build [`Client`]
pub struct ClientBuilder {
    mint_init: MintClientInit,
    wallet_init: WalletClientInit,
    ln_init: LnInit,
    stopped: bool,
}

impl ClientBuilder {
    pub(crate) fn new() -> Self {
        trace!(
            target: LOG_CLIENT,
            version = %env!("CARGO_PKG_VERSION"),
            "Initializing picomint client",
        );

        ClientBuilder {
            mint_init: MintClientInit,
            wallet_init: WalletClientInit,
            ln_init: LnInit::Regular(LightningClientInit::default()),
            stopped: false,
        }
    }

    /// Configure the regular lightning client flavor with a specific init
    /// (e.g. to override the gateway connection).
    pub fn with_lightning_init(&mut self, init: LightningClientInit) {
        self.ln_init = LnInit::Regular(init);
    }

    /// Mount the gateway lightning flavor. Used by the gateway daemon in
    /// place of the regular lightning module.
    pub fn with_gateway_ln(&mut self, gateway: Arc<dyn IGatewayClient>) {
        self.ln_init = LnInit::Gateway(GatewayClientInit { gateway });
    }

    pub fn stopped(&mut self) {
        self.stopped = true;
    }

    pub async fn load_existing_config(&self, db: &Database) -> anyhow::Result<ConsensusConfig> {
        let Some(config) = Client::get_config_from_db(db).await else {
            bail!("Client database not initialized")
        };

        Ok(config)
    }

    async fn init(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: ConsensusConfig,
    ) -> anyhow::Result<ClientHandle> {
        if Client::is_initialized(&db_no_decoders).await {
            bail!("Client database already initialized")
        }

        {
            debug!(target: LOG_CLIENT, "Initializing client database");
            let dbtx = db_no_decoders.begin_write().await;
            let tx = dbtx.as_ref();
            tx.insert(&CLIENT_CONFIG, &(), &config);
            dbtx.commit().await;
        }

        let stopped = self.stopped;
        self.build(connectors, db_no_decoders, pre_root_secret, config, stopped)
            .await
    }

    pub async fn preview(
        self,
        connectors: Endpoint,
        invite_code: &InviteCode,
    ) -> anyhow::Result<ClientPreview> {
        let (config, _api) = download_from_invite_code(&connectors, invite_code).await?;

        Ok(ClientPreview {
            connectors,
            inner: self,
            config,
        })
    }

    /// Use [`Self::preview`] instead
    pub async fn preview_with_existing_config(
        self,
        connectors: Endpoint,
        config: ConsensusConfig,
    ) -> anyhow::Result<ClientPreview> {
        Ok(ClientPreview {
            connectors,
            inner: self,
            config,
        })
    }

    pub async fn open(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let Some(config) = Client::get_config_from_db(&db_no_decoders).await else {
            bail!("Client database not initialized")
        };

        let pre_root_secret = pre_root_secret.to_inner(config.calculate_federation_id());

        let stopped = self.stopped;

        let client = self
            .build_stopped(connectors, db_no_decoders, pre_root_secret, &config)
            .await?;
        if !stopped {
            client.start_executor();
        }
        Ok(client)
    }

    /// Build a [`Client`] and start the executor
    pub(crate) async fn build(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: ConsensusConfig,
        stopped: bool,
    ) -> anyhow::Result<ClientHandle> {
        let client = self
            .build_stopped(connectors, db_no_decoders, pre_root_secret, &config)
            .await?;
        if !stopped {
            client.start_executor();
        }

        Ok(client)
    }

    // TODO: remove config argument
    /// Build a [`Client`] but do not start the executor
    async fn build_stopped(
        self,
        connectors: Endpoint,
        db_no_decoders: Database,
        pre_root_secret: DerivableSecret,
        config: &ConsensusConfig,
    ) -> anyhow::Result<ClientHandle> {
        debug!(
            target: LOG_CLIENT,
            version = %env!("CARGO_PKG_VERSION"),
            "Building picomint client",
        );
        let config = config.clone();
        let fed_id = config.calculate_federation_id();
        let db = db_no_decoders;
        let peer_node_ids: BTreeMap<PeerId, iroh_base::PublicKey> = config
            .iroh_endpoints
            .iter()
            .map(|(peer, endpoints)| (*peer, endpoints.node_id))
            .collect();
        let api: FederationApi = FederationApi::new(connectors.clone(), peer_node_ids).into();

        let task_group = TaskGroup::new();

        let final_client: LateClient = Arc::new(std::sync::OnceLock::new());

        let root_secret = Self::federation_root_secret(&pre_root_secret, &config);

        let mint_context = crate::module::ClientContext::new(
            final_client.clone(),
            ModuleKind::Mint,
            api.clone(),
            ApiScope::Mint,
            db.clone(),
            db.isolate("mint".to_string()),
            config.clone(),
            fed_id,
        );
        let mint = Arc::new(
            self.mint_init
                .init(
                    fed_id,
                    config.mint.clone(),
                    mint_context,
                    &root_secret.derive_module_secret(ModuleKind::Mint),
                    &task_group,
                )
                .await?,
        );

        let wallet_context = crate::module::ClientContext::new(
            final_client.clone(),
            ModuleKind::Wallet,
            api.clone(),
            ApiScope::Wallet,
            db.clone(),
            db.isolate("wallet".to_string()),
            config.clone(),
            fed_id,
        );
        let wallet = Arc::new(
            self.wallet_init
                .init(
                    config.wallet.clone(),
                    wallet_context,
                    &root_secret.derive_module_secret(ModuleKind::Wallet),
                    &task_group,
                )
                .await?,
        );

        let ln = match self.ln_init {
            LnInit::Regular(init) => {
                let ln_context = crate::module::ClientContext::new(
                    final_client.clone(),
                    ModuleKind::Ln,
                    api.clone(),
                    ApiScope::Ln,
                    db.clone(),
                    db.isolate("ln".to_string()),
                    config.clone(),
                    fed_id,
                );
                LnFlavor::Regular(Arc::new(
                    init.init(
                        fed_id,
                        config.ln.clone(),
                        ln_context,
                        &root_secret.derive_module_secret(ModuleKind::Ln),
                        &task_group,
                    )
                    .await?,
                ))
            }
            LnInit::Gateway(init) => {
                let ln_context = crate::module::ClientContext::new(
                    final_client.clone(),
                    ModuleKind::Ln,
                    api.clone(),
                    ApiScope::Ln,
                    db.clone(),
                    db.isolate("ln".to_string()),
                    config.clone(),
                    fed_id,
                );
                LnFlavor::Gateway(Arc::new(
                    init.init(
                        fed_id,
                        config.ln.clone(),
                        ln_context,
                        &root_secret.derive_module_secret(ModuleKind::Ln),
                        &task_group,
                    )
                    .await?,
                ))
            }
        };

        let tx_submission_executor = ModuleExecutor::new(
            db.clone(),
            TxSubmissionSmContext { api: api.clone() },
            task_group.clone(),
        );

        let client_inner = Arc::new(Client {
            config: tokio::sync::RwLock::new(config.clone()),
            db: db.clone(),
            connectors,
            federation_id: fed_id,
            federation_config_meta: config.meta,
            mint,
            wallet,
            ln,
            tx_submission_executor,
            api: api.clone(),
            secp_ctx: Secp256k1::new(),
            task_group: task_group.clone(),
        });

        let client_iface = std::sync::Arc::<Client>::downgrade(&client_inner);

        let client_arc = ClientHandle::new(client_inner);

        final_client
            .set(client_iface.clone())
            .expect("LateClient already set");

        client_arc.tx_submission_executor.start().await;

        // Module `start` is called *after* `final_client.set` so that any
        // per-module executors spawned here can safely resolve the weak
        // client reference from their transition contexts.
        client_arc.mint.start().await;
        client_arc.wallet.start().await;
        client_arc.ln.start().await;

        Ok(client_arc)
    }

    /// Re-derive client's `root_secret` using the federation ID. This
    /// eliminates the possibility of having the same client `root_secret`
    /// across multiple federations.
    fn federation_root_secret(
        pre_root_secret: &DerivableSecret,
        config: &ConsensusConfig,
    ) -> DerivableSecret {
        pre_root_secret.federation_key(&config.calculate_federation_id())
    }
}

/// An intermediate step before Client joining.
///
/// Meant to support showing user some initial information about the Federation
/// before actually joining.
pub struct ClientPreview {
    inner: ClientBuilder,
    config: ConsensusConfig,
    connectors: Endpoint,
}

impl ClientPreview {
    /// Get the config
    pub fn config(&self) -> &ConsensusConfig {
        &self.config
    }

    /// Join a new Federation
    pub async fn join(
        self,
        db_no_decoders: Database,
        pre_root_secret: RootSecret,
    ) -> anyhow::Result<ClientHandle> {
        let pre_root_secret = pre_root_secret.to_inner(self.config.calculate_federation_id());

        self.inner
            .init(
                self.connectors,
                db_no_decoders,
                pre_root_secret,
                self.config,
            )
            .await
    }
}
