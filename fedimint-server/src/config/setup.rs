use std::collections::{BTreeMap, BTreeSet};
use std::iter::once;
use std::sync::Arc;

use anyhow::{Context, ensure};
use async_trait::async_trait;
use fedimint_core::base32::FEDIMINT_PREFIX;
use fedimint_core::config::META_FEDERATION_NAME_KEY;
use fedimint_core::core::{ModuleInstanceId, ModuleKind};
use fedimint_core::db::Database;
use fedimint_core::envs::{FM_DISABLE_BASE_FEES_ENV, is_env_var_set};
use fedimint_core::module::{ApiAuth, ApiEndpoint, ApiEndpointContext, ApiRequestErased};
use fedimint_core::setup_code::PeerEndpoints;
use fedimint_core::{PeerId, base32};
use fedimint_server_core::dashboard_ui::SetupStatus;
use fedimint_server_core::setup_ui::ISetupApi;
use iroh::SecretKey;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;

use crate::config::{ConfigGenParams, ConfigGenSettings, PeerSetupCode};
use crate::net::HasApiContext;

/// State held by the API after receiving a `ConfigGenConnectionsRequest`
#[derive(Debug, Clone, Default)]
pub struct SetupState {
    /// Our local connection
    local_params: Option<LocalParams>,
    /// Connection info received from other guardians
    setup_codes: BTreeSet<PeerSetupCode>,
}

#[derive(Clone, Debug)]
/// Connection information sent between peers in order to start config gen
pub struct LocalParams {
    /// Secret key for our iroh api endpoint
    iroh_api_sk: iroh::SecretKey,
    /// Secret key for our iroh p2p endpoint
    iroh_p2p_sk: iroh::SecretKey,
    /// Our api and p2p endpoint
    endpoints: PeerEndpoints,
    /// Name of the peer
    name: String,
    /// Federation name set by the leader
    federation_name: Option<String>,
    /// Whether to disable base fees, set by the leader
    disable_base_fees: Option<bool>,
    /// Modules enabled by the leader (if None, all available modules are
    /// enabled)
    enabled_modules: Option<BTreeSet<ModuleKind>>,
    /// Total number of guardians (including the one who sets this), set by the
    /// leader
    federation_size: Option<u32>,
}

impl LocalParams {
    pub fn setup_code(&self) -> PeerSetupCode {
        PeerSetupCode {
            name: self.name.clone(),
            endpoints: self.endpoints.clone(),
            federation_name: self.federation_name.clone(),
            disable_base_fees: self.disable_base_fees,
            enabled_modules: self.enabled_modules.clone(),
            federation_size: self.federation_size,
        }
    }
}

/// Serves the config gen API endpoints
#[derive(Clone)]
pub struct SetupApi {
    /// Our config gen settings configured locally
    settings: ConfigGenSettings,
    /// In-memory state machine
    state: Arc<Mutex<SetupState>>,
    /// DB not really used
    db: Database,
    /// Triggers the distributed key generation
    sender: Sender<ConfigGenParams>,
    /// Guardian auth for authentication
    auth: ApiAuth,
}

impl SetupApi {
    pub fn new(
        settings: ConfigGenSettings,
        db: Database,
        sender: Sender<ConfigGenParams>,
        auth: ApiAuth,
    ) -> Self {
        Self {
            settings,
            state: Arc::new(Mutex::new(SetupState::default())),
            db,
            sender,
            auth,
        }
    }

    pub async fn setup_status(&self) -> SetupStatus {
        match self.state.lock().await.local_params {
            Some(..) => SetupStatus::SharingConnectionCodes,
            None => SetupStatus::AwaitingLocalParams,
        }
    }
}

#[async_trait]
impl ISetupApi for SetupApi {
    async fn setup_code(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .local_params
            .as_ref()
            .map(|lp| base32::encode_prefixed(FEDIMINT_PREFIX, &lp.setup_code()))
    }

    async fn guardian_name(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .local_params
            .as_ref()
            .map(|lp| lp.name.clone())
    }

    async fn auth(&self) -> ApiAuth {
        self.auth.clone()
    }

    async fn connected_peers(&self) -> Vec<String> {
        self.state
            .lock()
            .await
            .setup_codes
            .clone()
            .into_iter()
            .map(|info| info.name)
            .collect()
    }

    fn available_modules(&self) -> BTreeSet<ModuleKind> {
        self.settings.available_modules.clone()
    }

    fn default_modules(&self) -> BTreeSet<ModuleKind> {
        self.settings.default_modules.clone()
    }

    async fn reset_setup_codes(&self) {
        self.state.lock().await.setup_codes.clear();
    }

    async fn set_local_parameters(
        &self,
        name: String,
        federation_name: Option<String>,
        disable_base_fees: Option<bool>,
        enabled_modules: Option<BTreeSet<ModuleKind>>,
        federation_size: Option<u32>,
    ) -> anyhow::Result<String> {
        if let Some(existing_local_parameters) = self.state.lock().await.local_params.clone()
            && existing_local_parameters.name == name
            && existing_local_parameters.federation_name == federation_name
            && existing_local_parameters.disable_base_fees == disable_base_fees
            && existing_local_parameters.enabled_modules == enabled_modules
            && existing_local_parameters.federation_size == federation_size
        {
            return Ok(base32::encode_prefixed(
                FEDIMINT_PREFIX,
                &existing_local_parameters.setup_code(),
            ));
        }

        ensure!(!name.is_empty(), "The guardian name is empty");

        if let Some(federation_name) = federation_name.as_ref() {
            ensure!(!federation_name.is_empty(), "The federation name is empty");
        }

        if federation_name.is_some() {
            ensure!(
                federation_size.is_some(),
                "The leader must set the federation size"
            );
        }

        if let Some(size) = federation_size {
            ensure!(size >= 4, "Federation size must be at least 4");
        }

        let mut state = self.state.lock().await;

        ensure!(
            state.local_params.is_none(),
            "Local parameters have already been set"
        );

        let iroh_api_sk = SecretKey::from_bytes(&rand::random());
        let iroh_p2p_sk = SecretKey::from_bytes(&rand::random());

        let lp = LocalParams {
            iroh_api_sk: iroh_api_sk.clone(),
            iroh_p2p_sk: iroh_p2p_sk.clone(),
            endpoints: PeerEndpoints {
                api_pk: iroh_api_sk.public(),
                p2p_pk: iroh_p2p_sk.public(),
            },
            name,
            federation_name,
            disable_base_fees,
            enabled_modules,
            federation_size,
        };

        state.local_params = Some(lp.clone());

        Ok(base32::encode_prefixed(FEDIMINT_PREFIX, &lp.setup_code()))
    }

    async fn add_peer_setup_code(&self, info: String) -> anyhow::Result<String> {
        let info = base32::decode_prefixed(FEDIMINT_PREFIX, &info)?;

        let mut state = self.state.lock().await;

        if state.setup_codes.contains(&info) {
            return Ok(info.name.clone());
        }

        let local_params = state
            .local_params
            .clone()
            .expect("The endpoint is authenticated.");

        ensure!(
            info != local_params.setup_code(),
            "You cannot add your own setup code"
        );

        if let Some(federation_name) = state
            .setup_codes
            .iter()
            .chain(once(&local_params.setup_code()))
            .find_map(|info| info.federation_name.clone())
        {
            ensure!(
                info.federation_name.is_none(),
                "Federation name has already been set to {federation_name}"
            );
        }

        if let Some(disable_base_fees) = state
            .setup_codes
            .iter()
            .chain(once(&local_params.setup_code()))
            .find_map(|info| info.disable_base_fees)
        {
            ensure!(
                info.disable_base_fees.is_none(),
                "Base fees setting has already been configured to disabled={disable_base_fees}"
            );
        }

        if state
            .setup_codes
            .iter()
            .chain(once(&local_params.setup_code()))
            .any(|info| info.enabled_modules.is_some())
        {
            ensure!(
                info.enabled_modules.is_none(),
                "Enabled modules have already been configured by another guardian"
            );
        }

        if let Some(federation_size) = state
            .setup_codes
            .iter()
            .chain(once(&local_params.setup_code()))
            .find_map(|info| info.federation_size)
        {
            ensure!(
                info.federation_size.is_none(),
                "Federation size has already been set to {federation_size}"
            );
        }

        state.setup_codes.insert(info.clone());

        Ok(info.name)
    }

    async fn start_dkg(&self) -> anyhow::Result<()> {
        let mut state = self.state.lock().await.clone();

        let local_params = state
            .local_params
            .clone()
            .expect("The endpoint is authenticated.");

        let our_setup_code = local_params.setup_code();

        state.setup_codes.insert(our_setup_code.clone());

        ensure!(
            state.setup_codes.len() >= 4,
            "Federation size must be at least 4"
        );

        if let Some(federation_size) = state
            .setup_codes
            .iter()
            .find_map(|info| info.federation_size)
        {
            ensure!(
                state.setup_codes.len() == federation_size as usize,
                "Expected {federation_size} guardians but got {}",
                state.setup_codes.len()
            );
        }

        let federation_name = state
            .setup_codes
            .iter()
            .find_map(|info| info.federation_name.clone())
            .context("We need one guardian to configure the federations name")?;

        let disable_base_fees = state
            .setup_codes
            .iter()
            .find_map(|info| info.disable_base_fees)
            .unwrap_or(is_env_var_set(FM_DISABLE_BASE_FEES_ENV));

        let enabled_modules = state
            .setup_codes
            .iter()
            .find_map(|info| info.enabled_modules.clone())
            .unwrap_or_else(|| self.settings.default_modules.clone());

        let our_id = state
            .setup_codes
            .iter()
            .position(|info| info == &our_setup_code)
            .expect("We inserted the key above.");

        let params = ConfigGenParams {
            identity: PeerId::from(our_id as u16),
            iroh_api_sk: local_params.iroh_api_sk,
            iroh_p2p_sk: local_params.iroh_p2p_sk,
            peers: (0..)
                .map(|i| PeerId::from(i as u16))
                .zip(state.setup_codes.clone().into_iter())
                .collect(),
            meta: BTreeMap::from_iter(vec![(
                META_FEDERATION_NAME_KEY.to_string(),
                federation_name,
            )]),
            disable_base_fees,
            enabled_modules,
            network: self.settings.network,
        };

        self.sender
            .send(params)
            .await
            .context("Failed to send config gen params")?;

        Ok(())
    }

    async fn federation_size(&self) -> Option<u32> {
        let state = self.state.lock().await;
        let local_setup_code = state.local_params.as_ref().map(LocalParams::setup_code);
        state
            .setup_codes
            .iter()
            .chain(local_setup_code.iter())
            .find_map(|info| info.federation_size)
    }

    async fn cfg_federation_name(&self) -> Option<String> {
        let state = self.state.lock().await;
        let local_setup_code = state.local_params.as_ref().map(LocalParams::setup_code);
        state
            .setup_codes
            .iter()
            .chain(local_setup_code.iter())
            .find_map(|info| info.federation_name.clone())
    }

    async fn cfg_base_fees_disabled(&self) -> Option<bool> {
        let state = self.state.lock().await;
        let local_setup_code = state.local_params.as_ref().map(LocalParams::setup_code);
        state
            .setup_codes
            .iter()
            .chain(local_setup_code.iter())
            .find_map(|info| info.disable_base_fees)
    }

    async fn cfg_enabled_modules(&self) -> Option<BTreeSet<ModuleKind>> {
        let state = self.state.lock().await;
        let local_setup_code = state.local_params.as_ref().map(LocalParams::setup_code);
        state
            .setup_codes
            .iter()
            .chain(local_setup_code.iter())
            .find_map(|info| info.enabled_modules.clone())
    }
}

#[async_trait]
impl HasApiContext<SetupApi> for SetupApi {
    async fn context(
        &self,
        request: &ApiRequestErased,
        id: Option<ModuleInstanceId>,
    ) -> (&SetupApi, ApiEndpointContext) {
        assert!(id.is_none());

        let db = self.db.clone();

        let is_authenticated = request
            .auth
            .as_ref()
            .is_some_and(|auth| self.auth.verify(auth.as_str()));

        let context = ApiEndpointContext::new(db, is_authenticated, request.auth.clone());

        (self, context)
    }
}

pub fn server_endpoints() -> Vec<ApiEndpoint<SetupApi>> {
    vec![]
}
