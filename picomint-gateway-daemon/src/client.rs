use std::sync::Arc;

use iroh::Endpoint;
use iroh::endpoint::presets::N0;
use picomint_bip39::Mnemonic;
use picomint_client::{Client, RootSecret};
use picomint_client::gw::IGatewayClient;
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

    fn root_secret(&self) -> RootSecret {
        RootSecret::StandardDoubleDerive(picomint_bip39::to_root_secret(&self.mnemonic))
    }

    fn client_database(&self, federation_id: FederationId) -> Database {
        self.db.isolate(format!("client-{federation_id}"))
    }

    async fn save_config(&self, config: &ConsensusConfig) {
        let dbtx = self.db.begin_write().await;
        dbtx.as_ref()
            .insert(&CLIENT_CONFIG, &config.calculate_federation_id(), config);
        dbtx.commit().await;
    }

    /// Join a federation for the first time.
    pub async fn join(
        &self,
        invite: &InviteCode,
        gateway: Arc<AppState>,
    ) -> anyhow::Result<picomint_client::ClientHandleArc> {
        let federation_id = invite.federation_id();

        // Idempotent: if already joined, just load
        if let Some(client) = self.load(&federation_id, gateway.clone()).await? {
            return Ok(client);
        }

        let client = Client::join_gateway(
            self.connectors.clone(),
            self.client_database(federation_id),
            self.root_secret(),
            invite,
            gateway as Arc<dyn IGatewayClient>,
        )
        .await
        .map(Arc::new)
        .map_err(|e| anyhow::anyhow!("Client creation error: {e}"))?;

        self.save_config(&client.config().await).await;

        Ok(client)
    }

    /// Load an existing client for a federation.
    pub async fn load(
        &self,
        federation_id: &FederationId,
        gateway: Arc<AppState>,
    ) -> anyhow::Result<Option<picomint_client::ClientHandleArc>> {
        let db = self.client_database(*federation_id);

        if !Client::is_initialized(&db).await {
            return Ok(None);
        }

        let client = Client::open_gateway(
            self.connectors.clone(),
            db,
            self.root_secret(),
            gateway as Arc<dyn IGatewayClient>,
        )
        .await
        .map(Arc::new)
        .map_err(|e| anyhow::anyhow!("Client open error: {e}"))?;

        self.save_config(&client.config().await).await;

        Ok(Some(client))
    }

    /// List all federation configs stored in the database.
    pub async fn list_federations(&self) -> Vec<(FederationId, ConsensusConfig)> {
        self.db.begin_read().await.as_ref().iter(&CLIENT_CONFIG)
    }
}
