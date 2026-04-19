use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::presets::N0;
use picomint_client::gw::IGatewayClient;
use picomint_client::{Client, Mnemonic};
use picomint_core::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::invite_code::InviteCode;
use picomint_redb::Database;

use crate::AppState;
use crate::db::{CLIENT_CONFIG, ROOT_ENTROPY};

#[derive(Debug, Clone)]
pub struct GatewayClientFactory {
    db: Database,
    mnemonic: Mnemonic,
    connectors: Endpoint,
}

impl GatewayClientFactory {
    /// Initialize a new factory, storing the mnemonic entropy in the database.
    pub async fn init(db: Database, mnemonic: Mnemonic) -> anyhow::Result<Self> {
        let dbtx = db.begin_write().await;
        assert!(
            dbtx.as_ref()
                .insert(&ROOT_ENTROPY, &(), &mnemonic.to_entropy())
                .is_none()
        );
        dbtx.commit().await;

        let endpoint = Endpoint::builder(N0).bind().await?;

        Ok(Self {
            connectors: endpoint,
            db,
            mnemonic,
        })
    }

    /// Try to load an existing factory from the database.
    pub async fn try_load(db: Database) -> anyhow::Result<Option<Self>> {
        let entropy = db.begin_read().await.as_ref().get(&ROOT_ENTROPY, &());

        match entropy {
            Some(entropy) => {
                let mnemonic = Mnemonic::from_entropy(&entropy)
                    .map_err(|e| anyhow::anyhow!("Invalid stored mnemonic: {e}"))?;

                let endpoint = Endpoint::builder(N0).bind().await?;

                Ok(Some(Self {
                    connectors: endpoint,
                    db,
                    mnemonic,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn mnemonic(&self) -> &Mnemonic {
        &self.mnemonic
    }

    fn client_database(&self, federation_id: FederationId) -> Database {
        self.db.isolate(format!("client-{federation_id}"))
    }

    async fn read_config(&self, federation_id: &FederationId) -> Option<ConsensusConfig> {
        self.db
            .begin_read()
            .await
            .as_ref()
            .get(&CLIENT_CONFIG, federation_id)
    }

    /// Join a federation for the first time. Errors if a config for this
    /// federation is already persisted — use [`Self::load`] in that case.
    pub async fn join(
        &self,
        invite: &InviteCode,
        gateway: Arc<AppState>,
    ) -> anyhow::Result<picomint_client::ClientHandleArc> {
        let config = picomint_client::download(&self.connectors, invite).await?;

        let dbtx = self.db.begin_write().await;

        if dbtx
            .as_ref()
            .insert(&CLIENT_CONFIG, &config.calculate_federation_id(), &config)
            .is_some()
        {
            anyhow::bail!("Federation is already joined");
        }

        dbtx.commit().await;

        self.open(config, gateway).await
    }

    /// Open the client for a federation whose config is already persisted.
    /// Returns `None` if no config is stored for `federation_id`.
    pub async fn load(
        &self,
        federation_id: &FederationId,
        gateway: Arc<AppState>,
    ) -> anyhow::Result<Option<picomint_client::ClientHandleArc>> {
        match self.read_config(federation_id).await {
            Some(config) => self.open(config, gateway).await.map(Some),
            None => Ok(None),
        }
    }

    async fn open(
        &self,
        config: ConsensusConfig,
        gateway: Arc<AppState>,
    ) -> anyhow::Result<picomint_client::ClientHandleArc> {
        Client::new_gateway(
            self.connectors.clone(),
            self.client_database(config.calculate_federation_id()),
            &self.mnemonic,
            config,
            gateway as Arc<dyn IGatewayClient>,
        )
        .await
        .map(Arc::new)
        .map_err(|e| anyhow::anyhow!("Client open error: {e}"))
    }

    /// List all federation ids the gateway has joined.
    pub async fn list_federations(&self) -> Vec<FederationId> {
        self.db
            .begin_read()
            .await
            .as_ref()
            .iter(&CLIENT_CONFIG)
            .into_iter()
            .map(|(federation_id, _config)| federation_id)
            .collect()
    }
}
