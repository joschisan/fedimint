use std::sync::Arc;

use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::{Client, ClientBuilder, RootSecret};
use fedimint_client_module::secret::RootSecretStrategy;
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::invite_code::InviteCode;
use fedimint_gwv2_client::GatewayClientInitV2;
use iroh::Endpoint;
use iroh::endpoint::presets::N0;

use crate::AppState;
use crate::db::{ClientConfigKey, DbKeyPrefix, RootEntropyKey};

#[derive(Debug, Clone)]
pub struct GatewayClientFactory {
    db: Database,
    mnemonic: Mnemonic,
    registry: ClientModuleInitRegistry,
    connectors: Endpoint,
}

impl GatewayClientFactory {
    /// Initialize a new factory, storing the mnemonic entropy in the database.
    pub async fn init(
        db: Database,
        mnemonic: Mnemonic,
        registry: ClientModuleInitRegistry,
    ) -> anyhow::Result<Self> {
        let mut dbtx = db.begin_transaction().await;
        dbtx.insert_new_entry(&RootEntropyKey, &mnemonic.to_entropy())
            .await;
        dbtx.commit_tx_result()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to store mnemonic: {e}"))?;

        let endpoint = Endpoint::builder(N0).bind().await?;

        Ok(Self {
            connectors: endpoint,
            db,
            mnemonic,
            registry,
        })
    }

    /// Try to load an existing factory from the database.
    pub async fn try_load(
        db: Database,
        registry: ClientModuleInitRegistry,
    ) -> anyhow::Result<Option<Self>> {
        let entropy = db
            .begin_transaction_nc()
            .await
            .get_value(&RootEntropyKey)
            .await;

        match entropy {
            Some(entropy) => {
                let mnemonic = Mnemonic::from_entropy(&entropy)
                    .map_err(|e| anyhow::anyhow!("Invalid stored mnemonic: {e}"))?;

                let endpoint = Endpoint::builder(N0).bind().await?;

                Ok(Some(Self {
                    connectors: endpoint,
                    db,
                    mnemonic,
                    registry,
                }))
            }
            None => Ok(None),
        }
    }

    pub fn mnemonic(&self) -> &Mnemonic {
        &self.mnemonic
    }

    fn root_secret(&self) -> RootSecret {
        RootSecret::StandardDoubleDerive(Bip39RootSecretStrategy::<12>::to_root_secret(
            &self.mnemonic,
        ))
    }

    fn client_database(&self, federation_id: FederationId) -> Database {
        self.db.with_prefix(
            std::iter::once(DbKeyPrefix::ClientDatabase as u8)
                .chain(federation_id.consensus_encode_to_vec())
                .collect::<Vec<u8>>(),
        )
    }

    async fn client_builder(&self, gateway: Arc<AppState>) -> anyhow::Result<ClientBuilder> {
        let mut registry = self.registry.clone();
        registry.attach(GatewayClientInitV2 { gateway });

        let mut builder = Client::builder()
            .await
            .map_err(|e| anyhow::anyhow!("Client creation error: {e}"))?;
        builder.with_module_inits(registry);
        Ok(builder)
    }

    async fn save_config(&self, config: &ClientConfig) {
        let mut dbtx = self.db.begin_transaction().await;
        dbtx.insert_entry(&ClientConfigKey(config.calculate_federation_id()), config)
            .await;
        dbtx.commit_tx().await;
    }

    /// Join a federation for the first time.
    pub async fn join(
        &self,
        invite: &InviteCode,
        gateway: Arc<AppState>,
    ) -> anyhow::Result<fedimint_client::ClientHandleArc> {
        let federation_id = invite.federation_id();

        // Idempotent: if already joined, just load
        if let Some(client) = self.load(&federation_id, gateway.clone()).await? {
            return Ok(client);
        }

        let builder = self.client_builder(gateway).await?;

        let client = builder
            .preview(self.connectors.clone(), invite)
            .await?
            .join(self.client_database(federation_id), self.root_secret())
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
    ) -> anyhow::Result<Option<fedimint_client::ClientHandleArc>> {
        let db = self.client_database(*federation_id);

        if !Client::is_initialized(&db).await {
            return Ok(None);
        }

        let builder = self.client_builder(gateway).await?;

        let client = builder
            .open(self.connectors.clone(), db, self.root_secret())
            .await
            .map(Arc::new)
            .map_err(|e| anyhow::anyhow!("Client open error: {e}"))?;

        self.save_config(&client.config().await).await;

        Ok(Some(client))
    }

    /// List all federation configs stored in the database.
    pub async fn list_federations(&self) -> Vec<(FederationId, ClientConfig)> {
        use futures::StreamExt;

        use crate::db::ClientConfigPrefix;

        self.db
            .begin_transaction_nc()
            .await
            .find_by_prefix(&ClientConfigPrefix)
            .await
            .map(|(key, config)| (key.0, config))
            .collect()
            .await
    }
}

// Need this for consensus_encode_to_vec
use fedimint_core::encoding::Encodable;
