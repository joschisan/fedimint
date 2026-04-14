use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, bail};
use bitcoin::hex::DisplayHex as _;
use fedimint_client_module::db::ClientModuleMigrationFn;
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_client_module::sm::{ActiveStateMeta, InactiveStateMeta};
use fedimint_core::config::ClientConfig;
use fedimint_core::core::{ModuleInstanceId, OperationId};
use fedimint_core::db::{
    Database, DatabaseTransaction, DatabaseVersion, DatabaseVersionKey,
    IDatabaseTransactionOpsCore, IDatabaseTransactionOpsCoreTyped, MODULE_GLOBAL_PREFIX,
    apply_migrations_dbtx, create_database_version_dbtx, get_current_database_version,
};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::{impl_db_lookup, impl_db_record};
use fedimint_logging::LOG_CLIENT_DB;
use futures::StreamExt;
use serde::Serialize;
use strum::IntoEnumIterator as _;
use strum_macros::EnumIter;
use tracing::{debug, info, trace, warn};

use crate::sm::executor::{
    ActiveStateKeyBytes, ActiveStateKeyPrefixBytes, ExecutorDbPrefixes, InactiveStateKeyBytes,
    InactiveStateKeyPrefixBytes,
};

#[repr(u8)]
#[derive(Clone, EnumIter, Debug)]
pub enum DbKeyPrefix {
    EncodedClientSecret = 0x28,
    ClientPreRootSecretHash = 0x2a,
    OperationLog = 0x2c,
    ClientConfig = 0x2f,
    ClientInitState = 0x31,
    ApiSecret = 0x36,
    ApiUrlAnnouncement = 0x38,
    EventLog = fedimint_eventlog::DB_KEY_PREFIX_EVENT_LOG,
    UnorderedEventLog = fedimint_eventlog::DB_KEY_PREFIX_UNORDERED_EVENT_LOG,
    EventLogTrimable = fedimint_eventlog::DB_KEY_PREFIX_EVENT_LOG_TRIMABLE,
    ClientModuleRecovery = 0x40,
    GuardianMetadata = 0x42,

    DatabaseVersion = fedimint_core::db::DbKeyPrefix::DatabaseVersion as u8,

    ActiveStates = ExecutorDbPrefixes::ActiveStates as u8,
    InactiveStates = ExecutorDbPrefixes::InactiveStates as u8,

    /// Arbitrary data of the applications integrating Fedimint client and
    /// wanting to store some Federation-specific data in Fedimint client
    /// database.
    ///
    /// New users are encouraged to use this single prefix only.
    //
    // TODO: https://github.com/fedimint/fedimint/issues/4444
    //       in the future, we should make all global access to the db private
    //       and only expose a getter returning isolated database.
    UserData = 0xb0,
    /// Prefixes between 0xb1..=0xcf shall all be considered allocated for
    /// historical and future external use
    ExternalReservedStart = 0xb1,
    /// Prefixes between 0xb1..=0xcf shall all be considered allocated for
    /// historical and future external use
    ExternalReservedEnd = 0xcf,
    /// 0xd0.. reserved for Fedimint internal use
    // (see [`DbKeyPrefixInternalReserved`] for *internal* details)
    InternalReservedStart = 0xd0,
    /// Per-module instance data
    ModuleGlobalPrefix = 0xff,
}

#[repr(u8)]
#[derive(Clone, EnumIter, Debug)]
pub(crate) enum DbKeyPrefixInternalReserved {
    /// [`crate::Client::built_in_application_event_log_tracker`]
    DefaultApplicationEventLogPos = 0xd0,
}

pub(crate) async fn verify_client_db_integrity_dbtx(dbtx: &mut DatabaseTransaction<'_>) {
    let prefixes: BTreeSet<u8> = DbKeyPrefix::iter().map(|prefix| prefix as u8).collect();

    let mut records = dbtx.raw_find_by_prefix(&[]).await.expect("DB fail");
    while let Some((k, v)) = records.next().await {
        // from here and above, we don't want to spend time verifying it
        if DbKeyPrefix::UserData as u8 <= k[0] {
            break;
        }

        assert!(
            prefixes.contains(&k[0]),
            "Unexpected client db record found: {}: {}",
            k.as_hex(),
            v.as_hex()
        );
    }
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Encodable, Decodable)]
pub struct EncodedClientSecretKey;

#[derive(Debug, Encodable, Decodable)]
pub struct EncodedClientSecretKeyPrefix;

impl_db_record!(
    key = EncodedClientSecretKey,
    value = Vec<u8>,
    db_prefix = DbKeyPrefix::EncodedClientSecret,
);
impl_db_lookup!(
    key = EncodedClientSecretKey,
    query_prefix = EncodedClientSecretKeyPrefix
);

#[derive(Debug, Encodable, Decodable, Serialize)]
pub struct ClientPreRootSecretHashKey;

impl_db_record!(
    key = ClientPreRootSecretHashKey,
    value = [u8; 8],
    db_prefix = DbKeyPrefix::ClientPreRootSecretHash
);

#[derive(Debug, Encodable, Decodable, Serialize)]
pub struct ClientConfigKey;

impl_db_record!(
    key = ClientConfigKey,
    value = ClientConfig,
    db_prefix = DbKeyPrefix::ClientConfig
);

#[derive(Debug, Encodable, Decodable, Serialize)]
pub struct ApiSecretKey;

#[derive(Debug, Encodable)]
pub struct ApiSecretKeyPrefix;

impl_db_record!(
    key = ApiSecretKey,
    value = String,
    db_prefix = DbKeyPrefix::ApiSecret
);

impl_db_lookup!(key = ApiSecretKey, query_prefix = ApiSecretKeyPrefix);

/// Does the client modules need to run recovery before being usable?
#[derive(Debug, Encodable, Decodable, Serialize)]
pub struct ClientInitStateKey;

#[derive(Debug, Encodable)]
pub struct ClientInitStatePrefix;

/// Client initialization mode
#[derive(Debug, Encodable, Decodable)]
pub enum InitMode {
    /// Should only be used with freshly generated root secret
    Fresh,
    /// Should be used with root secrets provided by the user to recover a
    /// (even if just possibly) already used secret.
    Recover,
}

/// Like `InitMode`, but without no longer required data.
///
/// This is distinct from `InitMode` to prevent holding on to `snapshot`
/// forever both for user's privacy and space use. In case user get hacked
/// or phone gets stolen.
#[derive(Debug, Encodable, Decodable)]
pub enum InitModeComplete {
    Fresh,
    Recover,
}

/// The state of the client initialization
#[derive(Debug, Encodable, Decodable)]
pub enum InitState {
    /// Client data initialization might still require some work (e.g. client
    /// recovery)
    Pending(InitMode),
    /// Client initialization was complete
    Complete(InitModeComplete),
}

impl InitState {
    pub fn into_complete(self) -> Self {
        match self {
            InitState::Pending(p) => InitState::Complete(match p {
                InitMode::Fresh => InitModeComplete::Fresh,
                InitMode::Recover => InitModeComplete::Recover,
            }),
            InitState::Complete(t) => InitState::Complete(t),
        }
    }

    pub fn does_require_recovery(&self) -> bool {
        matches!(self, InitState::Pending(InitMode::Recover))
    }

    pub fn is_pending(&self) -> bool {
        match self {
            InitState::Pending(_) => true,
            InitState::Complete(_) => false,
        }
    }
}

impl_db_record!(
    key = ClientInitStateKey,
    value = InitState,
    db_prefix = DbKeyPrefix::ClientInitState
);

impl_db_lookup!(
    key = ClientInitStateKey,
    query_prefix = ClientInitStatePrefix
);

#[derive(Debug, Encodable, Decodable, Serialize)]
pub struct ClientModuleRecovery {
    pub module_instance_id: ModuleInstanceId,
}

#[derive(Debug, Clone, Encodable, Decodable)]
pub struct ClientModuleRecoveryState {
    pub progress: RecoveryProgress,
}

impl ClientModuleRecoveryState {
    pub fn is_done(&self) -> bool {
        self.progress.is_done()
    }
}

impl_db_record!(
    key = ClientModuleRecovery,
    value = ClientModuleRecoveryState,
    db_prefix = DbKeyPrefix::ClientModuleRecovery,
);

pub fn get_core_client_database_migrations()
-> BTreeMap<DatabaseVersion, fedimint_core::db::ClientCoreDbMigrationFn> {
    BTreeMap::new()
}

/// Apply core client database migrations
///
/// TODO: This should be private.
pub async fn apply_migrations_core_client_dbtx(
    dbtx: &mut DatabaseTransaction<'_>,
    kind: String,
) -> Result<(), anyhow::Error> {
    apply_migrations_dbtx(
        dbtx,
        (),
        kind,
        get_core_client_database_migrations(),
        None,
        Some(DbKeyPrefix::UserData as u8),
    )
    .await
}

/// `apply_migrations_client` iterates from the on disk database version for the
/// client module up to `target_db_version` and executes all of the migrations
/// that exist in the migrations map, including state machine migrations.
/// Each migration in the migrations map updates the database to have the
/// correct on-disk data structures that the code is expecting. The entire
/// process is atomic, (i.e migration from 0->1 and 1->2 happen atomically).
/// This function is called before the module is initialized and as long as the
/// correct migrations are supplied in the migrations map, the module
/// will be able to read and write from the database successfully.
pub async fn apply_migrations_client_module(
    db: &Database,
    kind: String,
    migrations: BTreeMap<DatabaseVersion, ClientModuleMigrationFn>,
    module_instance_id: ModuleInstanceId,
) -> Result<(), anyhow::Error> {
    let mut dbtx = db.begin_transaction().await;
    apply_migrations_client_module_dbtx(
        &mut dbtx.to_ref_nc(),
        kind,
        migrations,
        module_instance_id,
    )
    .await?;
    dbtx.commit_tx_result()
        .await
        .map_err(|e| anyhow::Error::msg(e.to_string()))
}

pub async fn apply_migrations_client_module_dbtx(
    dbtx: &mut DatabaseTransaction<'_>,
    kind: String,
    migrations: BTreeMap<DatabaseVersion, ClientModuleMigrationFn>,
    module_instance_id: ModuleInstanceId,
) -> Result<(), anyhow::Error> {
    // Newly created databases will not have any data underneath the
    // `MODULE_GLOBAL_PREFIX` since they have just been instantiated.
    let is_new_db = dbtx
        .raw_find_by_prefix(&[MODULE_GLOBAL_PREFIX])
        .await?
        .next()
        .await
        .is_none();

    let target_version = get_current_database_version(&migrations);

    // First write the database version to disk if it does not exist.
    create_database_version_dbtx(
        dbtx,
        target_version,
        Some(module_instance_id),
        kind.clone(),
        is_new_db,
    )
    .await?;

    let current_version = dbtx
        .get_value(&DatabaseVersionKey(module_instance_id))
        .await;

    let db_version = if let Some(mut current_version) = current_version {
        if current_version == target_version {
            trace!(
                target: LOG_CLIENT_DB,
                %current_version,
                %target_version,
                module_instance_id,
                kind,
                "Database version up to date"
            );
            return Ok(());
        }

        if target_version < current_version {
            return Err(anyhow!(format!(
                "On disk database version for module {kind} was higher ({}) than the target database version ({}).",
                current_version, target_version,
            )));
        }

        info!(
            target: LOG_CLIENT_DB,
            %current_version,
            %target_version,
            module_instance_id,
            kind,
            "Migrating client module database"
        );
        let mut active_states = get_active_states(&mut dbtx.to_ref_nc(), module_instance_id).await;
        let mut inactive_states =
            get_inactive_states(&mut dbtx.to_ref_nc(), module_instance_id).await;

        while current_version < target_version {
            let new_states = if let Some(migration) = migrations.get(&current_version) {
                debug!(
                     target: LOG_CLIENT_DB,
                     module_instance_id,
                     %kind,
                     %current_version,
                     %target_version,
                     "Running module db migration");

                migration(
                    &mut dbtx
                        .to_ref_with_prefix_module_id(module_instance_id)
                        .0
                        .into_nc(),
                    active_states.clone(),
                    inactive_states.clone(),
                )
                .await?
            } else {
                warn!(
                    target: LOG_CLIENT_DB,
                    ?current_version, "Missing client db migration");
                None
            };

            // If the client migration returned new states, a state machine migration has
            // occurred, and the new states need to be persisted to the database.
            if let Some((new_active_states, new_inactive_states)) = new_states {
                remove_old_and_persist_new_active_states(
                    &mut dbtx.to_ref_nc(),
                    new_active_states.clone(),
                    active_states.clone(),
                    module_instance_id,
                )
                .await;
                remove_old_and_persist_new_inactive_states(
                    &mut dbtx.to_ref_nc(),
                    new_inactive_states.clone(),
                    inactive_states.clone(),
                    module_instance_id,
                )
                .await;

                // the new states become the old states for the next migration
                active_states = new_active_states;
                inactive_states = new_inactive_states;
            }

            current_version = current_version.increment();
            dbtx.insert_entry(&DatabaseVersionKey(module_instance_id), &current_version)
                .await;
        }

        current_version
    } else {
        target_version
    };

    debug!(
        target: LOG_CLIENT_DB,
        ?kind, ?db_version, "Client DB Version");
    Ok(())
}

/// Reads all active states from the database and returns `Vec<DynState>`.
/// TODO: It is unfortunate that we can't read states by the module's instance
/// id so we are forced to return all active states. Once we do a db migration
/// to add `module_instance_id` to `ActiveStateKey`, this can be improved to
/// only read the module's relevant states.
pub async fn get_active_states(
    dbtx: &mut DatabaseTransaction<'_>,
    module_instance_id: ModuleInstanceId,
) -> Vec<(Vec<u8>, OperationId)> {
    dbtx.find_by_prefix(&ActiveStateKeyPrefixBytes)
        .await
        .filter_map(|(state, _)| async move {
            if module_instance_id == state.module_instance_id {
                Some((state.state, state.operation_id))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .await
}

/// Reads all inactive states from the database and returns `Vec<DynState>`.
/// TODO: It is unfortunate that we can't read states by the module's instance
/// id so we are forced to return all inactive states. Once we do a db migration
/// to add `module_instance_id` to `InactiveStateKey`, this can be improved to
/// only read the module's relevant states.
pub async fn get_inactive_states(
    dbtx: &mut DatabaseTransaction<'_>,
    module_instance_id: ModuleInstanceId,
) -> Vec<(Vec<u8>, OperationId)> {
    dbtx.find_by_prefix(&InactiveStateKeyPrefixBytes)
        .await
        .filter_map(|(state, _)| async move {
            if module_instance_id == state.module_instance_id {
                Some((state.state, state.operation_id))
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
        .await
}

/// Persists new active states by first removing all current active states, and
/// re-writing with the new set of active states. `new_active_states` is
/// expected to contain all active states, not just the newly created states.
pub async fn remove_old_and_persist_new_active_states(
    dbtx: &mut DatabaseTransaction<'_>,
    new_active_states: Vec<(Vec<u8>, OperationId)>,
    states_to_remove: Vec<(Vec<u8>, OperationId)>,
    module_instance_id: ModuleInstanceId,
) {
    // Remove all existing active states
    for (bytes, operation_id) in states_to_remove {
        dbtx.remove_entry(&ActiveStateKeyBytes {
            operation_id,
            module_instance_id,
            state: bytes,
        })
        .await
        .expect("Did not delete anything");
    }

    // Insert new "migrated" active states
    for (bytes, operation_id) in new_active_states {
        dbtx.insert_new_entry(
            &ActiveStateKeyBytes {
                operation_id,
                module_instance_id,
                state: bytes,
            },
            &ActiveStateMeta::default(),
        )
        .await;
    }
}

/// Persists new inactive states by first removing all current inactive states,
/// and re-writing with the new set of inactive states. `new_inactive_states` is
/// expected to contain all inactive states, not just the newly created states.
pub async fn remove_old_and_persist_new_inactive_states(
    dbtx: &mut DatabaseTransaction<'_>,
    new_inactive_states: Vec<(Vec<u8>, OperationId)>,
    states_to_remove: Vec<(Vec<u8>, OperationId)>,
    module_instance_id: ModuleInstanceId,
) {
    // Remove all existing active states
    for (bytes, operation_id) in states_to_remove {
        dbtx.remove_entry(&InactiveStateKeyBytes {
            operation_id,
            module_instance_id,
            state: bytes,
        })
        .await
        .expect("Did not delete anything");
    }

    // Insert new "migrated" inactive states
    for (bytes, operation_id) in new_inactive_states {
        dbtx.insert_new_entry(
            &InactiveStateKeyBytes {
                operation_id,
                module_instance_id,
                state: bytes,
            },
            &InactiveStateMeta {
                created_at: fedimint_core::time::now(),
                exited_at: fedimint_core::time::now(),
            },
        )
        .await;
    }
}

/// Fetches the encoded client secret from the database and decodes it.
/// If an encoded client secret is not present in the database, or if
/// decoding fails, an error is returned.
pub async fn get_decoded_client_secret<T: Decodable>(db: &Database) -> anyhow::Result<T> {
    let mut tx = db.begin_transaction_nc().await;
    let client_secret = tx.get_value(&EncodedClientSecretKey).await;

    match client_secret {
        Some(client_secret) => {
            T::consensus_decode_whole(&client_secret, &ModuleRegistry::default())
                .map_err(|e| anyhow!("Decoding failed: {e}"))
        }
        None => bail!("Encoded client secret not present in DB"),
    }
}

