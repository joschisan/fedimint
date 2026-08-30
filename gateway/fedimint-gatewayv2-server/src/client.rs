use std::fmt::Debug;
use std::path::Path;
use std::sync::Arc;

use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::{Client, ClientBuilder, RootSecret};
use fedimint_client_module::secret::RootSecretStrategy;
use fedimint_connectors::ConnectorRegistry;
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_gwv2_client::GatewayClientInitV2;
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientInit as MintV2ClientInit;
use rand::rngs::OsRng;
use tracing::debug;

use crate::DB_FILE;
use crate::db::{RootEntropyKey, get_client_database};

/// Reads the gateway's mnemonic from the persisted BIP39 root entropy, if it
/// has been established.
pub async fn load_mnemonic(gateway_db: &Database) -> Option<Mnemonic> {
    let entropy = gateway_db
        .begin_transaction_nc()
        .await
        .get_value(&RootEntropyKey)
        .await?;

    Mnemonic::from_entropy(&entropy).ok()
}

/// Factory that builds a Fedimint client for each joined federation, modeled
/// on picomint's `GatewayClientFactory`.
#[derive(Debug, Clone)]
pub struct GatewayClientFactory {
    db: Database,
    registry: ClientModuleInitRegistry,
    connectors: ConnectorRegistry,
}

impl GatewayClientFactory {
    /// Opens the gateway database, ensures the root-entropy mnemonic exists,
    /// and builds the client factory: everything the gateway needs to build
    /// federation clients (and to derive the LDK node's entropy), assembled
    /// before the node is built.
    ///
    /// `gatewaydv2` has no setup UI and never imports a seed: it always boots
    /// with a freshly generated mnemonic, persisted once as the root entropy.
    /// The gateway's identity keypair and all per-federation client secrets
    /// derive from this single root entropy.
    pub async fn prepare(data_dir: &Path) -> anyhow::Result<(Database, Self, Mnemonic)> {
        let gateway_db = Database::new(
            fedimint_rocksdb::RocksDb::build(data_dir.join(DB_FILE))
                .open()
                .await?,
            ModuleDecoderRegistry::default(),
        );

        if load_mnemonic(&gateway_db).await.is_none() {
            debug!(target: LOG_GATEWAY, "Generating mnemonic and writing root entropy to storage");
            let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut OsRng);
            let mut dbtx = gateway_db.begin_transaction().await;
            dbtx.insert_entry(&RootEntropyKey, &mnemonic.to_entropy())
                .await;
            dbtx.commit_tx().await;
        }
        let mnemonic = load_mnemonic(&gateway_db)
            .await
            .expect("root entropy was just ensured");

        let mut registry = ClientModuleInitRegistry::new();
        registry.attach(MintV2ClientInit);
        registry.attach(fedimint_walletv2_client::WalletClientInit);
        // The v2 gateway orchestrates payments in the daemon itself, so no
        // daemon-side callback interface is provided; see
        // `GatewayClientInitV2::gateway`.
        registry.attach(GatewayClientInitV2 { gateway: None });

        let factory = Self {
            db: gateway_db.clone(),
            connectors: ConnectorRegistry::build_from_client_env()?.bind().await?,
            registry,
        };

        Ok((gateway_db, factory, mnemonic))
    }

    /// Constructs the client builder with the modules, database, and connector
    /// used to create clients for connected federations.
    async fn create_client_builder(&self) -> anyhow::Result<ClientBuilder> {
        let mut client_builder = Client::builder()
            .await
            .map_err(|e| anyhow::anyhow!("Failed to create a federation client: {e}"))?
            .with_iroh_enable_dht(true);
        client_builder.with_module_inits(self.registry.clone());
        Ok(client_builder)
    }

    /// Downloads a federation's `ClientConfig` from its invite without building
    /// or joining a client. Used at connect time to persist the config; the
    /// client itself is built lazily on first use.
    pub async fn download_config(&self, invite_code: &InviteCode) -> anyhow::Result<ClientConfig> {
        let preview = self
            .create_client_builder()
            .await?
            .preview(self.connectors.clone(), invite_code)
            .await?;
        Ok(preview.config().clone())
    }

    /// Opens an existing federation client, or joins it from the stored
    /// `ClientConfig` if its database has not been initialized yet. Clients are
    /// built lazily on first use; connecting only persists the config.
    pub async fn open(
        &self,
        federation_id: FederationId,
        config: ClientConfig,
        mnemonic: &Mnemonic,
    ) -> anyhow::Result<fedimint_client::ClientHandleArc> {
        let db = get_client_database(&self.db, &federation_id);

        // Matches Conduit's derivation, so the gateway's funds in a federation
        // can be recovered by entering its mnemonic into Conduit.
        let root_secret = RootSecret::StandardDoubleDerive(
            Bip39RootSecretStrategy::<12>::to_root_secret(mnemonic),
        );

        let client_builder = self.create_client_builder().await?;

        if Client::is_initialized(&db).await {
            client_builder
                .open(self.connectors.clone(), db, root_secret)
                .await
        } else {
            // The api secret is intentionally not stored or handled; lazy joins
            // use `None`.
            client_builder
                .preview_with_existing_config(self.connectors.clone(), config, None)
                .await?
                .join(db, root_secret)
                .await
        }
        .map(Arc::new)
        .map_err(|e| anyhow::anyhow!("Failed to create a federation client: {e}"))
    }
}
