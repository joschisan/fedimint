use std::collections::BTreeMap;
use std::iter::once;
use std::sync::Arc;

use anyhow::{Context, ensure};
use iroh::SecretKey;
use picomint_core::PeerId;
use picomint_core::config::META_FEDERATION_NAME_KEY;
use picomint_encoding::{Decodable, Encodable};
use serde::Serialize;
use tokio::sync::Mutex;
use tokio::sync::mpsc::Sender;

use crate::config::{ConfigGenParams, ConfigGenSettings, ServerConfig, SetupResult};

/// Connection information sent between peers in order to start config gen.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Encodable, Decodable, Serialize)]
pub struct PeerSetupCode {
    /// Name of the peer
    pub name: String,
    /// Public key of the peer's single iroh endpoint (serves both p2p and
    /// client-API traffic, demuxed by node-id on accept).
    pub pk: iroh_base::PublicKey,
    /// Federation name set by the leader
    pub federation_name: Option<String>,
    /// Total number of guardians (including the one who sets this), set by the
    /// leader
    pub federation_size: Option<u32>,
}

/// The state of the server while config gen is running.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SetupStatus {
    /// Waiting for guardian to set the local parameters
    AwaitingLocalParams,
    /// Sharing the connection codes with our peers
    SharingConnectionCodes,
    /// Consensus is running
    ConsensusIsRunning,
}

/// State held by the setup API after receiving a set of local parameters.
#[derive(Debug, Clone, Default)]
pub struct SetupState {
    /// Our local connection
    local_params: Option<LocalParams>,
    /// Connection info received from other guardians
    setup_codes: std::collections::BTreeSet<PeerSetupCode>,
}

#[derive(Clone, Debug)]
/// Connection information sent between peers in order to start config gen
pub struct LocalParams {
    /// Secret key for our single iroh endpoint (p2p + api)
    iroh_sk: iroh::SecretKey,
    /// Name of the peer
    name: String,
    /// Federation name set by the leader
    federation_name: Option<String>,
    /// Total number of guardians (including the one who sets this), set by the
    /// leader
    federation_size: Option<u32>,
}

impl LocalParams {
    pub fn setup_code(&self) -> PeerSetupCode {
        PeerSetupCode {
            name: self.name.clone(),
            pk: self.iroh_sk.public(),
            federation_name: self.federation_name.clone(),
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
    /// Signals the setup loop with either DKG params or a restored config
    sender: Sender<SetupResult>,
}

impl SetupApi {
    pub fn new(settings: ConfigGenSettings, sender: Sender<SetupResult>) -> Self {
        Self {
            settings,
            state: Arc::new(Mutex::new(SetupState::default())),
            sender,
        }
    }

    pub async fn setup_code(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .local_params
            .as_ref()
            .map(|lp| picomint_base32::encode(&lp.setup_code()))
    }

    pub async fn guardian_name(&self) -> Option<String> {
        self.state
            .lock()
            .await
            .local_params
            .as_ref()
            .map(|lp| lp.name.clone())
    }

    pub async fn connected_peers(&self) -> Vec<String> {
        self.state
            .lock()
            .await
            .setup_codes
            .clone()
            .into_iter()
            .map(|info| info.name)
            .collect()
    }

    pub async fn reset_setup_codes(&self) {
        self.state.lock().await.setup_codes.clear();
    }

    pub async fn setup_status(&self) -> SetupStatus {
        match self.state.lock().await.local_params {
            Some(..) => SetupStatus::SharingConnectionCodes,
            None => SetupStatus::AwaitingLocalParams,
        }
    }

    pub async fn set_local_parameters(
        &self,
        name: String,
        federation_name: Option<String>,
        federation_size: Option<u32>,
    ) -> anyhow::Result<String> {
        if let Some(existing_local_parameters) = self.state.lock().await.local_params.clone()
            && existing_local_parameters.name == name
            && existing_local_parameters.federation_name == federation_name
            && existing_local_parameters.federation_size == federation_size
        {
            return Ok(picomint_base32::encode(
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

        let iroh_sk = SecretKey::from_bytes(&rand::random());

        let lp = LocalParams {
            iroh_sk,
            name,
            federation_name,
            federation_size,
        };

        state.local_params = Some(lp.clone());

        Ok(picomint_base32::encode(&lp.setup_code()))
    }

    pub async fn add_peer_setup_code(&self, info: String) -> anyhow::Result<String> {
        let info = picomint_base32::decode(&info)?;

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

    pub async fn start_dkg(&self) -> anyhow::Result<()> {
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

        let our_id = state
            .setup_codes
            .iter()
            .position(|info| info == &our_setup_code)
            .expect("We inserted the key above.");

        let params = ConfigGenParams {
            identity: PeerId::from(our_id as u8),
            iroh_sk: local_params.iroh_sk,
            peers: (0..)
                .map(|i| PeerId::from(i as u8))
                .zip(state.setup_codes.clone().into_iter())
                .collect(),
            meta: BTreeMap::from_iter(vec![(
                META_FEDERATION_NAME_KEY.to_string(),
                federation_name,
            )]),
            network: self.settings.network,
        };

        self.sender
            .send(SetupResult::Dkg(params))
            .await
            .context("Failed to send config gen params")?;

        Ok(())
    }

    pub async fn restore_config(&self, cfg: ServerConfig) -> anyhow::Result<()> {
        cfg.validate_config(&cfg.private.identity)
            .context("Restored config failed validation")?;

        self.sender
            .send(SetupResult::Restored(Box::new(cfg)))
            .await
            .context("Failed to send restored config")?;

        Ok(())
    }

    pub async fn federation_size(&self) -> Option<u32> {
        let state = self.state.lock().await;
        let local_setup_code = state.local_params.as_ref().map(LocalParams::setup_code);
        state
            .setup_codes
            .iter()
            .chain(local_setup_code.iter())
            .find_map(|info| info.federation_size)
    }

    pub async fn cfg_federation_name(&self) -> Option<String> {
        let state = self.state.lock().await;
        let local_setup_code = state.local_params.as_ref().map(LocalParams::setup_code);
        state
            .setup_codes
            .iter()
            .chain(local_setup_code.iter())
            .find_map(|info| info.federation_name.clone())
    }
}
