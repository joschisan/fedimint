use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::Context;
use erased_serde::Serialize;
use fedimint_client::db::ClientConfigKeyPrefix;
use fedimint_client::module::init::ClientModuleInitRegistry;
use fedimint_core::config::{ClientConfig, CommonModuleInitRegistry, ServerModuleInitRegistry};
use fedimint_core::core::ModuleKind;
use fedimint_core::db::notifications::Notifications;
use fedimint_core::db::{DatabaseTransaction, DatabaseVersionKey, SingleUseDatabaseTransaction};
use fedimint_core::encoding::Encodable;
use fedimint_core::epoch::SerdeSignatureShare;
use fedimint_core::module::__reexports::serde_json;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::{push_db_pair_items, push_db_pair_items_no_serde};
use fedimint_rocksdb::RocksDbReadOnly;
use fedimint_server::config::io::read_server_config;
use fedimint_server::config::ServerConfig;
use fedimint_server::db as ConsensusRange;
use futures::StreamExt;
use ln_gateway::Gateway;
use strum::IntoEnumIterator;

#[derive(Debug, serde::Serialize)]
struct SerdeWrapper(#[serde(with = "hex::serde")] Vec<u8>);

impl SerdeWrapper {
    fn from_encodable<T: Encodable>(e: T) -> SerdeWrapper {
        let mut bytes = vec![];
        e.consensus_encode(&mut bytes)
            .expect("Write to vec can't fail");
        SerdeWrapper(bytes)
    }
}

/// Structure to hold the deserialized structs from the database.
/// Also includes metadata on which sections of the database to read.
pub struct DatabaseDump<'a> {
    serialized: BTreeMap<String, Box<dyn Serialize>>,
    read_only: DatabaseTransaction<'a>,
    modules: Vec<String>,
    prefixes: Vec<String>,
    cfg: Option<ServerConfig>,
    module_inits: ServerModuleInitRegistry,
    client_cfg: Option<ClientConfig>,
    client_module_inits: ClientModuleInitRegistry,
}

impl<'a> DatabaseDump<'a> {
    pub async fn new(
        cfg_dir: PathBuf,
        data_dir: String,
        password: String,
        module_inits: ServerModuleInitRegistry,
        client_module_inits: ClientModuleInitRegistry,
        modules: Vec<String>,
        prefixes: Vec<String>,
    ) -> anyhow::Result<DatabaseDump<'a>> {
        let read_only = match RocksDbReadOnly::open_read_only(data_dir.clone()) {
            Ok(db) => db,
            Err(_) => {
                panic!("Error reading RocksDB database. Quitting...");
            }
        };
        let single_use = SingleUseDatabaseTransaction::new(read_only);

        // leak here is OK, it only happens once.
        let notifications = Box::leak(Box::new(Notifications::new()));

        let (server_cfg, client_cfg, decoders) = if let Ok(cfg) =
            read_server_config(&password, cfg_dir).context("Failed to read server config")
        {
            // Successfully read the server's config, that means this database is a server
            // db
            let decoders = module_inits
                .available_decoders(cfg.iter_module_instances())
                .unwrap()
                .with_fallback();
            (Some(cfg), None, decoders)
        } else {
            // Check if this database is a client database by reading the `ClientConfig`
            // from the database.
            let read_only_client = match RocksDbReadOnly::open_read_only(data_dir) {
                Ok(db) => db,
                Err(_) => {
                    panic!("Error reading RocksDB database. Quitting...");
                }
            };

            let single_use = SingleUseDatabaseTransaction::new(read_only_client);
            let mut dbtx = DatabaseTransaction::new(
                Box::new(single_use),
                ModuleDecoderRegistry::default(),
                notifications,
            );
            let client_cfg = dbtx
                .find_by_prefix(&ClientConfigKeyPrefix)
                .await
                .next()
                .await
                .map(|(_, client_cfg)| client_cfg);

            if let Some(client_cfg) = client_cfg {
                // Successfully read the client config, that means this database is a client db
                let kinds = client_cfg.modules.iter().map(|(k, v)| (*k, &v.kind));
                let decoders = client_module_inits
                    .available_decoders(kinds)
                    .unwrap()
                    .with_fallback();
                (None, Some(client_cfg), decoders)
            } else {
                (None, None, ModuleDecoderRegistry::default())
            }
        };

        let dbtx = DatabaseTransaction::new(Box::new(single_use), decoders, notifications);

        Ok(DatabaseDump {
            serialized: BTreeMap::new(),
            read_only: dbtx,
            modules,
            prefixes,
            cfg: server_cfg,
            module_inits,
            client_module_inits,
            client_cfg,
        })
    }
}

impl<'a> DatabaseDump<'a> {
    /// Prints the contents of the BTreeMap to a pretty JSON string
    fn print_database(&self) {
        let json = serde_json::to_string_pretty(&self.serialized).unwrap();
        println!("{json}");
    }

    async fn serialize_module(
        &mut self,
        module_id: &u16,
        kind: &ModuleKind,
        inits: CommonModuleInitRegistry,
    ) -> anyhow::Result<()> {
        if !self.modules.is_empty() && !self.modules.contains(&kind.to_string()) {
            return Ok(());
        }
        let mut isolated_dbtx = self.read_only.with_module_prefix(*module_id);

        match inits.get(kind) {
            None => {
                tracing::warn!(module_id, %kind, "Detected configuration for unsupported module");

                let mut module_serialized = BTreeMap::new();
                let filtered_prefixes = (0u8..=255).filter(|f| {
                    self.prefixes.is_empty()
                        || self.prefixes.contains(&f.to_string().to_lowercase())
                });

                let isolated_dbtx = &mut isolated_dbtx;

                for prefix in filtered_prefixes {
                    let db_items = isolated_dbtx
                        .raw_find_by_prefix(&[prefix])
                        .await?
                        .map(|(k, v)| {
                            (
                                k.consensus_encode_to_hex().expect("can't fail"),
                                Box::new(v.consensus_encode_to_hex().expect("can't fail")),
                            )
                        })
                        .collect::<BTreeMap<String, Box<_>>>()
                        .await;

                    module_serialized.extend(db_items);
                }
                self.serialized
                    .insert(format!("{kind}-{module_id}"), Box::new(module_serialized));
            }
            Some(init) => {
                let mut module_serialized = init
                    .dump_database(&mut isolated_dbtx, self.prefixes.clone())
                    .await
                    .collect::<BTreeMap<String, _>>();

                let db_version = isolated_dbtx.get_value(&DatabaseVersionKey).await;
                if let Some(db_version) = db_version {
                    module_serialized.insert("Version".to_string(), Box::new(db_version));
                } else {
                    module_serialized
                        .insert("Version".to_string(), Box::new("Not Specified".to_string()));
                }

                self.serialized
                    .insert(format!("{kind}-{module_id}"), Box::new(module_serialized));
            }
        }

        Ok(())
    }

    async fn serialize_gateway(&mut self) -> anyhow::Result<()> {
        let mut dbtx = self.read_only.get_isolated();
        let gateway_serialized = Gateway::dump_database(&mut dbtx, self.prefixes.clone())
            .await
            .collect::<BTreeMap<String, _>>();
        self.serialized
            .insert("gateway".to_string(), Box::new(gateway_serialized));
        Ok(())
    }

    /// Iterates through all the specified ranges in the database and retrieves
    /// the data for each range. Prints serialized contents at the end.
    pub async fn dump_database(&mut self) -> anyhow::Result<()> {
        let cfg = self.cfg.clone();
        if let Some(cfg) = cfg {
            if self.modules.is_empty() || self.modules.contains(&"consensus".to_string()) {
                self.retrieve_consensus_data().await;
            }

            for (module_id, module_cfg) in &cfg.consensus.modules {
                let kind = &module_cfg.kind;
                self.serialize_module(module_id, kind, self.module_inits.to_common())
                    .await?;
            }

            self.print_database();
            return Ok(());
        }

        if let Some(cfg) = self.client_cfg.clone() {
            for (module_id, module_cfg) in &cfg.modules {
                let kind = &module_cfg.kind;
                let mut modules = Vec::new();
                if let Some(module) = self.client_module_inits.get(kind) {
                    modules.push(module.to_dyn_common());
                }

                let registry = CommonModuleInitRegistry::from(modules);
                self.serialize_module(module_id, kind, registry).await?;
            }

            self.print_database();
            return Ok(());
        }

        self.serialize_gateway().await?;
        self.print_database();

        Ok(())
    }

    /// Iterates through each of the prefixes within the consensus range and
    /// retrieves the corresponding data.
    async fn retrieve_consensus_data(&mut self) {
        let mut consensus: BTreeMap<String, Box<dyn Serialize>> = BTreeMap::new();
        let dbtx = &mut self.read_only;
        let prefix_names = &self.prefixes;

        let filtered_prefixes = ConsensusRange::DbKeyPrefix::iter().filter(|f| {
            prefix_names.is_empty() || prefix_names.contains(&f.to_string().to_lowercase())
        });
        for table in filtered_prefixes {
            match table {
                ConsensusRange::DbKeyPrefix::SessionIndex => {
                    if let Some(index) = dbtx.get_value(&ConsensusRange::SessionIndexKey).await {
                        consensus.insert("Client Config Signature".to_string(), Box::new(index));
                    }
                }
                ConsensusRange::DbKeyPrefix::AcceptedIndex => {
                    push_db_pair_items_no_serde!(
                        dbtx,
                        ConsensusRange::AcceptedIndexPrefix,
                        ConsensusRange::AcceptedIndexKey,
                        (),
                        consensus,
                        "Accepted Index"
                    );
                }
                ConsensusRange::DbKeyPrefix::AcceptedTransaction => {
                    push_db_pair_items_no_serde!(
                        dbtx,
                        ConsensusRange::AcceptedTransactionKeyPrefix,
                        ConsensusRange::AcceptedTransactionKey,
                        fedimint_server::consensus::AcceptedTransaction,
                        consensus,
                        "Accepted Transactions"
                    );
                }
                ConsensusRange::DbKeyPrefix::SignedBlock => {
                    push_db_pair_items_no_serde!(
                        dbtx,
                        ConsensusRange::SignedBlockPrefix,
                        ConsensusRange::SignedBlockKey,
                        fedimint_server::consensus::SignedBlock,
                        consensus,
                        "Signed Blocks"
                    );
                }
                ConsensusRange::DbKeyPrefix::AlephUnits => {
                    push_db_pair_items_no_serde!(
                        dbtx,
                        ConsensusRange::AlephUnitsPrefix,
                        ConsensusRange::AlephUnitsKey,
                        Vec<u8>,
                        consensus,
                        "Aleph Units"
                    );
                }
                ConsensusRange::DbKeyPrefix::ClientConfigSignature => {
                    let signature = dbtx
                        .get_value(&ConsensusRange::ClientConfigSignatureKey)
                        .await;

                    if let Some(signature) = signature {
                        consensus
                            .insert("Client Config Signature".to_string(), Box::new(signature));
                    }
                }
                ConsensusRange::DbKeyPrefix::ClientConfigSignatureShare => {
                    push_db_pair_items!(
                        dbtx,
                        ConsensusRange::ClientConfigSignatureSharePrefix,
                        ConsensusRange::ClientConfigSignatureShareKey,
                        SerdeSignatureShare,
                        consensus,
                        "Client Config Signature Share"
                    );
                }
                ConsensusRange::DbKeyPrefix::ClientConfigDownload => {
                    push_db_pair_items!(
                        dbtx,
                        ConsensusRange::ClientConfigDownloadKeyPrefix,
                        ConsensusRange::ClientConfigDownloadKey,
                        u64,
                        consensus,
                        "Client Config Download"
                    );
                }
                // Module is a global prefix for all module data
                ConsensusRange::DbKeyPrefix::Module => {}
            }
        }

        self.serialized
            .insert("Consensus".to_string(), Box::new(consensus));
    }
}
