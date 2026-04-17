use picomint_api_client::config::ConsensusConfig;
use picomint_client_module::module::recovery::RecoveryProgress;
use picomint_core::core::ModuleKind;
use picomint_core::encoding::{Decodable, Encodable};
use picomint_core::table;
use serde::Serialize;

table!(
    CLIENT_CONFIG,
    () => ConsensusConfig,
    "client-config",
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

picomint_core::consensus_value!(InitState);

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
    pub kind: ModuleKind,
}

picomint_core::consensus_key!(ClientModuleRecovery);

#[derive(Debug, Clone, Encodable, Decodable)]
pub struct ClientModuleRecoveryState {
    pub progress: RecoveryProgress,
}

picomint_core::consensus_value!(ClientModuleRecoveryState);

impl ClientModuleRecoveryState {
    pub fn is_done(&self) -> bool {
        self.progress.is_done()
    }
}
