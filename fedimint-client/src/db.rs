use anyhow::{anyhow, bail};
use fedimint_client_module::module::recovery::RecoveryProgress;
use fedimint_core::config::ClientConfig;
use fedimint_core::core::ModuleInstanceId;
use fedimint_core::db::IReadDatabaseTransactionOpsTyped as _;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::module::registry::ModuleRegistry;
use fedimint_core::table;
use fedimint_redb::Database;
use serde::Serialize;

table!(
    ENCODED_CLIENT_SECRET,
    () => Vec<u8>,
    "encoded-client-secret",
);

table!(
    CLIENT_PRE_ROOT_SECRET_HASH,
    () => [u8; 8],
    "client-pre-root-secret-hash",
);

table!(
    CLIENT_CONFIG,
    () => ClientConfig,
    "client-config",
);

table!(
    API_SECRET,
    () => String,
    "api-secret",
);

table!(
    CLIENT_INIT_STATE,
    () => InitState,
    "client-init-state",
);

table!(
    CLIENT_MODULE_RECOVERY,
    ClientModuleRecovery => ClientModuleRecoveryState,
    "client-module-recovery",
);

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

/// Fetches the encoded client secret from the database and decodes it.
/// If an encoded client secret is not present in the database, or if
/// decoding fails, an error is returned.
pub async fn get_decoded_client_secret<T: Decodable>(db: &Database) -> anyhow::Result<T> {
    let tx = db.begin_read().await;
    let client_secret = tx.as_ref().get(&ENCODED_CLIENT_SECRET, &());

    match client_secret {
        Some(client_secret) => {
            T::consensus_decode_whole(&client_secret, &ModuleRegistry::default())
                .map_err(|e| anyhow!("Decoding failed: {e}"))
        }
        None => bail!("Encoded client secret not present in DB"),
    }
}
