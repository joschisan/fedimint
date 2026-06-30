#![deny(clippy::pedantic)]
#![allow(clippy::cast_possible_truncation)]
#![allow(clippy::cast_possible_wrap)]
#![allow(clippy::cast_sign_loss)]
#![allow(clippy::default_trait_access)]
#![allow(clippy::doc_markdown)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::return_self_not_must_use)]
#![allow(clippy::similar_names)]
#![allow(clippy::too_many_lines)]
#![allow(clippy::large_futures)]
#![allow(clippy::struct_field_names)]

pub mod client;
pub mod config;
mod db;
pub mod envs;
mod error;
mod events;
mod federation_manager;
mod lightning;
mod metrics;
pub mod rpc_server;
mod types;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt::Display;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use bitcoin::hashes::sha256;
use bitcoin::{Address, Network, Txid, secp256k1};
use clap::Parser;
use client::GatewayClientBuilder;
pub use config::GatewayParameters;
use config::{DatabaseBackend, GatewayOpts};
use envs::FM_GATEWAY_SKIP_WAIT_FOR_SYNC_ENV;
use error::FederationNotConnected;
use events::ALL_GATEWAY_EVENTS;
use federation_manager::FederationManager;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_bitcoind::bitcoincore::BitcoindClient;
use fedimint_client::ClientHandleArc;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::db::Database;
use fedimint_core::envs::is_env_var_set;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::CommonModuleInit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::task::{TaskGroup, TaskHandle, TaskShutdownToken, sleep};
use fedimint_core::time::duration_since_epoch;
use fedimint_core::util::backoff_util::fibonacci_max_one_hour;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow, SafeUrl, Spanned, retry};
use fedimint_core::{
    Amount, BitcoinAmountOrAll, PeerId, crit, fedimint_build_code_version_env,
    get_network_for_address,
};
use fedimint_eventlog::{DBTransactionEventLogExt, EventLogId, StructuredPaymentEvents};
use fedimint_gateway_common::{
    BackupPayload, ChainSource, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse,
    ConnectFedPayload, ConnectorType, CreateInvoiceForOperatorPayload, CreateOfferPayload,
    CreateOfferResponse, DepositAddressPayload, DepositAddressRecheckPayload,
    FederationBalanceInfo, FederationConfig, FederationInfo, GatewayBalances, GatewayFedConfig,
    GatewayInfo, GetInvoiceRequest, GetInvoiceResponse, LeaveFedPayload, LightningInfo,
    LightningMode, ListTransactionsPayload, ListTransactionsResponse, MnemonicResponse,
    OpenChannelRequest, PayInvoiceForOperatorPayload, PayOfferPayload, PayOfferResponse,
    PaymentLogPayload, PaymentLogResponse, PaymentStats, PaymentSummaryPayload,
    PaymentSummaryResponse, PeginFromOnchainPayload, ReceiveEcashPayload, ReceiveEcashResponse,
    RegisteredProtocol, SendOnchainRequest, SetChannelFeesRequest, SpendEcashPayload,
    SpendEcashResponse, V1_API_ENDPOINT, WithdrawPayload, WithdrawResponse,
    WithdrawToOnchainPayload,
};
use fedimint_gwv2_client::events::compute_lnv2_stats;
use fedimint_gwv2_client::{
    EXPIRATION_DELTA_MINIMUM_V2, FinalReceiveState, GatewayClientModuleV2, IGatewayClientV2,
};
use fedimint_lightning::{InterceptPaymentResponse, LightningRpcError, PaymentAction, Preimage};
use fedimint_lnurl::VerifyResponse;
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::{
    CreateBolt11InvoicePayload, PaymentFee, RoutingInfo, SendPaymentPayload,
};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::{
    MintClientInit as MintV2ClientInit, MintClientModule as MintV2ClientModule,
};
use fedimint_wallet_common::PegOutFees;
use futures::stream::StreamExt;
use lightning_invoice::Bolt11Invoice;
use rand::rngs::OsRng;
use tokio::sync::RwLock;
use tracing::{debug, info, info_span, warn};

use crate::db::GatewayDbtxNcExt as _;
use crate::error::{AdminGatewayError, LNv2Error, PublicGatewayError};
use crate::events::get_events_for_duration;
use crate::lightning::{
    CreateInvoiceRequest, GatewayLdkClient, InterceptPaymentRequest, InvoiceDescription,
    LightningContext, RouteHtlcStream,
};
use crate::rpc_server::run_webserver;
use crate::types::PrettyInterceptPaymentRequest;

/// Default Bitcoin network for testing purposes.
pub const DEFAULT_NETWORK: Network = Network::Regtest;

pub type Result<T> = std::result::Result<T, PublicGatewayError>;
pub type AdminResult<T> = std::result::Result<T, AdminGatewayError>;

/// Name of the gateway's database that is used for metadata and configuration
/// storage.
const DB_FILE: &str = "gatewayd.db";

/// Name of the folder that the gateway uses to store its node database when
/// running in LDK mode.
const LDK_NODE_DB_FOLDER: &str = "ldk_node";

#[cfg_attr(doc, aquamarine::aquamarine)]
/// ```mermaid
/// graph LR
/// classDef virtual fill:#fff,stroke-dasharray: 5 5
///
///    Disconnected -- establish lightning connection --> Connected
///    Connected -- load federation clients --> Running
///    Connected -- not synced to chain --> Syncing
///    Syncing -- load federation clients --> Running
///    Running -- disconnected from lightning node --> Disconnected
///    Running -- shutdown initiated --> ShuttingDown
/// ```
#[derive(Clone, Debug)]
pub enum GatewayState {
    Disconnected,
    Syncing,
    Connected,
    Running { lightning_context: LightningContext },
    ShuttingDown { lightning_context: LightningContext },
}

impl Display for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            GatewayState::Disconnected => write!(f, "Disconnected"),
            GatewayState::Syncing => write!(f, "Syncing"),
            GatewayState::Connected => write!(f, "Connected"),
            GatewayState::Running { .. } => write!(f, "Running"),
            GatewayState::ShuttingDown { .. } => write!(f, "ShuttingDown"),
        }
    }
}

/// Helper struct for storing the gateway's advertised endpoint for each network
/// protocol. The advertised public key is derived on demand from the gateway's
/// mnemonic (see [`Gateway::gateway_id`]), so it is not stored here.
#[derive(Debug, Clone)]
struct Registration {
    /// The url to advertise in the registration that clients can use to connect
    endpoint_url: SafeUrl,
}

impl Registration {
    pub fn new(endpoint_url: SafeUrl) -> Self {
        Self { endpoint_url }
    }
}

#[bon::bon]
impl Gateway {
    /// Construct a [`Gateway`] using a fluent builder API.
    ///
    /// # Example
    /// ```ignore
    /// let gateway = Gateway::builder(lightning_mode, client_builder, gateway_db)
    ///     .listen(addr)
    ///     .api_addr(url)
    ///     .bcrypt_password_hash(hash)
    ///     .network(Network::Regtest)
    ///     .gateway_state(state)
    ///     .chain_source(chain_source)
    ///     .build()
    ///     .await?;
    /// ```
    #[builder(start_fn = builder, finish_fn = build)]
    pub fn new_with_builder(
        #[builder(start_fn)] lightning_mode: LightningMode,
        #[builder(start_fn)] client_builder: GatewayClientBuilder,
        #[builder(start_fn)] gateway_db: Database,
        bcrypt_password_hash: bcrypt::HashParts,
        bcrypt_liquidity_manager_password_hash: Option<bcrypt::HashParts>,
        gateway_state: GatewayState,
        chain_source: ChainSource,
        #[builder(default = ([127, 0, 0, 1], 80).into())] listen: SocketAddr,
        api_addr: Option<SafeUrl>,
        #[builder(default = DEFAULT_NETWORK)] network: Network,
        #[builder(default = PaymentFee::TRANSACTION_FEE_DEFAULT)] default_routing_fees: PaymentFee,
        #[builder(default = PaymentFee::TRANSACTION_FEE_DEFAULT)]
        default_transaction_fees: PaymentFee,
        metrics_listen: Option<SocketAddr>,
    ) -> anyhow::Result<Gateway> {
        let versioned_api = api_addr.map(|addr| {
            addr.join(V1_API_ENDPOINT)
                .expect("Failed to version gateway API address")
        });

        let metrics_listen = metrics_listen.unwrap_or_else(|| {
            SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                listen.port() + 1,
            )
        });

        Ok(Gateway::new(
            lightning_mode,
            GatewayParameters {
                listen,
                versioned_api,
                bcrypt_password_hash,
                bcrypt_liquidity_manager_password_hash,
                network,
                default_routing_fees,
                default_transaction_fees,
                skip_setup: true,
                metrics_listen,
            },
            gateway_db,
            client_builder,
            gateway_state,
            chain_source,
        ))
    }
}

/// The action to take after handling a payment stream.
enum ReceivePaymentStreamAction {
    RetryAfterDelay,
    NoRetry,
}

#[derive(Clone)]
pub struct Gateway {
    /// The gateway's federation manager.
    federation_manager: Arc<RwLock<FederationManager>>,

    /// The mode that specifies the lightning connection parameters
    lightning_mode: LightningMode,

    /// The current state of the Gateway.
    state: Arc<RwLock<GatewayState>>,

    /// Builder struct that allows the gateway to build a Fedimint client, which
    /// handles the communication with a federation.
    client_builder: GatewayClientBuilder,

    /// Database for Gateway metadata.
    gateway_db: Database,

    /// The socket the gateway listens on.
    listen: SocketAddr,

    /// The socket the gateway's metrics server listens on.
    metrics_listen: SocketAddr,

    /// The task group for all tasks related to the gateway.
    task_group: TaskGroup,

    /// The bcrypt password hash used to authenticate the gateway.
    bcrypt_password_hash: String,

    /// The bcrypt password hash used to authenticate the gateway liquidity
    /// manager.
    bcrypt_liquidity_manager_password_hash: Option<String>,

    /// The Bitcoin network that the Lightning network is configured to.
    network: Network,

    /// The source of the Bitcoin blockchain data
    chain_source: ChainSource,

    /// The default routing fees for new federations
    default_routing_fees: PaymentFee,

    /// The default transaction fees for new federations
    default_transaction_fees: PaymentFee,

    /// A map of the network protocols the gateway supports to the data needed
    /// for registering with a federation.
    registrations: BTreeMap<RegisteredProtocol, Registration>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("federation_manager", &self.federation_manager)
            .field("state", &self.state)
            .field("client_builder", &self.client_builder)
            .field("gateway_db", &self.gateway_db)
            .field("listen", &self.listen)
            .field("registrations", &self.registrations)
            .finish_non_exhaustive()
    }
}

/// Executes a withdrawal using the walletv2 module
async fn withdraw_v2(
    client: &ClientHandleArc,
    wallet_module: &fedimint_walletv2_client::WalletClientModule,
    address: &Address,
    amount: BitcoinAmountOrAll,
) -> AdminResult<WithdrawResponse> {
    let fee = wallet_module
        .send_fee()
        .await
        .map_err(|e| AdminGatewayError::WithdrawError {
            failure_reason: e.to_string(),
        })?;

    let withdraw_amount = match amount {
        BitcoinAmountOrAll::All => {
            let balance = bitcoin::Amount::from_sat(
                client
                    .get_balance_for_btc()
                    .await
                    .map_err(|err| {
                        AdminGatewayError::Unexpected(anyhow!(
                            "Balance not available: {}",
                            err.fmt_compact_anyhow()
                        ))
                    })?
                    .msats
                    / 1000,
            );
            balance
                .checked_sub(fee)
                .ok_or_else(|| AdminGatewayError::WithdrawError {
                    failure_reason: format!("Insufficient funds. Balance: {balance} Fee: {fee}"),
                })?
        }
        BitcoinAmountOrAll::Amount(a) => a,
    };

    let operation_id = wallet_module
        .send(
            address.as_unchecked().clone(),
            withdraw_amount,
            Some(fee),
            serde_json::Value::Null,
        )
        .await
        .map_err(|e| AdminGatewayError::WithdrawError {
            failure_reason: e.to_string(),
        })?;

    let result = wallet_module
        .await_final_send_operation_state(operation_id)
        .await
        .map_err(|e| AdminGatewayError::WithdrawError {
            failure_reason: e.to_string(),
        })?;

    let fees = PegOutFees::from_amount(fee);

    match result {
        fedimint_walletv2_client::FinalSendOperationState::Success(txid) => {
            info!(target: LOG_GATEWAY, amount = %withdraw_amount, address = %address, "Sent funds via walletv2");
            Ok(WithdrawResponse { txid, fees })
        }
        fedimint_walletv2_client::FinalSendOperationState::Aborted => {
            Err(AdminGatewayError::WithdrawError {
                failure_reason: "Withdrawal transaction was aborted".to_string(),
            })
        }
        fedimint_walletv2_client::FinalSendOperationState::Failure => {
            Err(AdminGatewayError::WithdrawError {
                failure_reason: "Withdrawal failed".to_string(),
            })
        }
    }
}

impl Gateway {
    /// Returns a bitcoind client using the credentials that were passed in from
    /// the environment variables.
    fn get_bitcoind_client(
        opts: &GatewayOpts,
        network: bitcoin::Network,
        gateway_id: &PublicKey,
    ) -> anyhow::Result<(BitcoindClient, ChainSource)> {
        let bitcoind_username = opts
            .bitcoind_username
            .clone()
            .expect("FM_BITCOIND_URL is set but FM_BITCOIND_USERNAME is not");
        let url = opts.bitcoind_url.clone().expect("No bitcoind url set");
        let password = opts
            .bitcoind_password
            .clone()
            .expect("FM_BITCOIND_URL is set but FM_BITCOIND_PASSWORD is not");

        let chain_source = ChainSource::Bitcoind {
            username: bitcoind_username.clone(),
            password: password.clone(),
            server_url: url.clone(),
        };
        let wallet_name = format!("gatewayd-{gateway_id}");
        let client = BitcoindClient::new(&url, bitcoind_username, password, &wallet_name, network)?;
        Ok((client, chain_source))
    }

    /// Default function for creating a gateway with the `Mint`, `Wallet`, and
    /// `Gateway` modules.
    pub async fn new_with_default_modules() -> anyhow::Result<Gateway> {
        let opts = GatewayOpts::parse();
        let gateway_parameters = opts.to_gateway_parameters()?;
        let decoders = ModuleDecoderRegistry::default();

        let db_path = opts.data_dir.join(DB_FILE);
        let gateway_db = match opts.db_backend {
            DatabaseBackend::RocksDb => {
                debug!(target: LOG_GATEWAY, "Using RocksDB database backend");
                Database::new(
                    fedimint_rocksdb::RocksDb::build(db_path).open().await?,
                    decoders,
                )
            }
            DatabaseBackend::CursedRedb => {
                debug!(target: LOG_GATEWAY, "Using CursedRedb database backend");
                Database::new(
                    fedimint_cursed_redb::MemAndRedb::new(db_path).await?,
                    decoders,
                )
            }
        };

        // `gatewaydv2` has no setup UI and never imports a seed: it always boots
        // with a freshly generated mnemonic, persisted once as the root entropy.
        // The gateway's identity keypair and all per-federation client secrets
        // derive from this single root entropy.
        if Self::load_mnemonic(&gateway_db).await.is_none() {
            debug!(target: LOG_GATEWAY, "Generating mnemonic and writing root entropy to storage");
            let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut OsRng);
            let mut dbtx = gateway_db.begin_transaction().await;
            dbtx.save_root_entropy(&mnemonic.to_entropy()).await;
            dbtx.commit_tx().await;
        }
        let gateway_state = GatewayState::Disconnected;

        // The gateway identity (derived from the mnemonic) doubles as the unique
        // identifier of the bitcoind watch-only wallet.
        let mnemonic = Self::load_mnemonic(&gateway_db)
            .await
            .expect("mnemonic was just established");
        let http_id = Self::derive_gateway_keypair(&mnemonic).public_key();
        let chain_source = match (opts.bitcoind_url.as_ref(), opts.esplora_url.as_ref()) {
            // Use bitcoind by default if both are set
            (Some(_), _) => {
                let (_, chain_source) =
                    Self::get_bitcoind_client(&opts, gateway_parameters.network, &http_id)?;
                chain_source
            }
            (None, Some(url)) => ChainSource::Esplora {
                server_url: url.clone(),
            },
            _ => unreachable!("ArgGroup already enforced XOR relation"),
        };

        // Gateway module will be attached when the federation clients are created
        // because the LN RPC will be injected with `GatewayClientGen`.
        let mut registry = ClientModuleInitRegistry::new();
        registry.attach(MintV2ClientInit);
        registry.attach(fedimint_walletv2_client::WalletClientInit);

        let client_builder =
            GatewayClientBuilder::new(opts.data_dir.clone(), registry, opts.db_backend).await?;

        info!(
            target: LOG_GATEWAY,
            version = %fedimint_build_code_version_env!(),
            "Starting gatewayd",
        );

        Ok(Gateway::new(
            opts.mode,
            gateway_parameters,
            gateway_db,
            client_builder,
            gateway_state,
            chain_source,
        ))
    }

    /// Helper function for creating a gateway from either
    /// `new_with_default_modules` or `Gateway::builder`.
    fn new(
        lightning_mode: LightningMode,
        gateway_parameters: GatewayParameters,
        gateway_db: Database,
        client_builder: GatewayClientBuilder,
        gateway_state: GatewayState,
        chain_source: ChainSource,
    ) -> Gateway {
        let network = gateway_parameters.network;

        let task_group = TaskGroup::new();
        task_group.install_kill_handler();

        let mut registrations = BTreeMap::new();
        if let Some(http_url) = gateway_parameters.versioned_api {
            registrations.insert(RegisteredProtocol::Http, Registration::new(http_url));
        }

        Self {
            federation_manager: Arc::new(RwLock::new(FederationManager::new())),
            lightning_mode,
            state: Arc::new(RwLock::new(gateway_state)),
            client_builder,
            gateway_db,
            listen: gateway_parameters.listen,
            metrics_listen: gateway_parameters.metrics_listen,
            task_group,
            bcrypt_password_hash: gateway_parameters.bcrypt_password_hash.to_string(),
            bcrypt_liquidity_manager_password_hash: gateway_parameters
                .bcrypt_liquidity_manager_password_hash
                .map(|h| h.to_string()),
            network,
            chain_source,
            default_routing_fees: gateway_parameters.default_routing_fees,
            default_transaction_fees: gateway_parameters.default_transaction_fees,
            registrations,
        }
    }

    /// Domain-separation tweak for deriving the gateway's identity keypair from
    /// the root secret. Stable across restarts so the gateway id (and the
    /// bitcoind watch-only wallet name) is reproducible from the mnemonic
    /// alone.
    const GATEWAY_IDENTITY_TWEAK: &'static [u8] = b"gatewayv2-identity";

    /// Deterministically derives the gateway's identity keypair from its
    /// mnemonic. The private half is never used for signing (LNv2 uses
    /// per-federation module keys); only the public key is advertised.
    fn derive_gateway_keypair(mnemonic: &Mnemonic) -> secp256k1::Keypair {
        Bip39RootSecretStrategy::<12>::to_root_secret(mnemonic)
            .tweak(Self::GATEWAY_IDENTITY_TWEAK)
            .to_secp_key(&secp256k1::Secp256k1::new())
    }

    /// Returns the gateway's advertised identity public key, or `None` if the
    /// mnemonic has not been set yet.
    async fn gateway_id(&self) -> Option<PublicKey> {
        let mnemonic = Self::load_mnemonic(&self.gateway_db).await?;
        Some(Self::derive_gateway_keypair(&mnemonic).public_key())
    }

    pub async fn http_gateway_id(&self) -> PublicKey {
        self.gateway_id().await.expect("mnemonic should be set")
    }

    async fn get_state(&self) -> GatewayState {
        self.state.read().await.clone()
    }

    /// Main entrypoint into the gateway that starts the client registration
    /// timer, loads the federation clients from the persisted config,
    /// begins listening for intercepted payments, and starts the webserver
    /// to service requests.
    pub async fn run(
        self,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> anyhow::Result<TaskShutdownToken> {
        install_crypto_provider().await;
        self.load_clients().await?;
        self.start_gateway(runtime);
        // start metrics server
        fedimint_metrics::spawn_api_server(self.metrics_listen, self.task_group.clone()).await?;
        // start webserver last to avoid handling requests before fully initialized
        let handle = self.task_group.make_handle();
        run_webserver(Arc::new(self)).await?;
        let shutdown_receiver = handle.make_shutdown_rx();
        Ok(shutdown_receiver)
    }

    /// Begins the task for listening for intercepted payments from the
    /// lightning node.
    fn start_gateway(&self, runtime: Arc<tokio::runtime::Runtime>) {
        const PAYMENT_STREAM_RETRY_SECONDS: u64 = 60;

        let self_copy = self.clone();
        let tg = self.task_group.clone();
        self.task_group.spawn(
            "Subscribe to intercepted lightning payments in stream",
            |handle| async move {
                // Repeatedly attempt to establish a connection to the lightning node and create a payment stream, re-trying if the connection is broken.
                loop {
                    if handle.is_shutting_down() {
                        info!(target: LOG_GATEWAY, "Gateway lightning payment stream handler loop is shutting down");
                        break;
                    }

                    let payment_stream_task_group = tg.make_subgroup();
                    let lnrpc_route = self_copy.create_lightning_client(runtime.clone()).await;

                    debug!(target: LOG_GATEWAY, "Establishing lightning payment stream...");
                    let (stream, ln_client) = match lnrpc_route.route_htlcs(&payment_stream_task_group).await
                    {
                        Ok((stream, ln_client)) => (stream, ln_client),
                        Err(err) => {
                            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to open lightning payment stream");
                            // `route_htlcs` may have already spawned tasks into the
                            // subgroup before failing (e.g. the LNv1 interceptor is
                            // spawned before LNv2 setup, which can fail). Tear the
                            // subgroup down so no stale task keeps owning the LND HTLC
                            // stream, which would prevent the retry from taking over and
                            // could cause it to cancel real HTLCs after `gateway_receiver`
                            // is dropped.
                            if let Err(err) = payment_stream_task_group.shutdown_join_all(None).await {
                                crit!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Lightning payment stream task group shutdown");
                            }
                            sleep(Duration::from_secs(PAYMENT_STREAM_RETRY_SECONDS)).await;
                            continue
                        }
                    };

                    // Successful calls to `route_htlcs` establish a connection
                    self_copy.set_gateway_state(GatewayState::Connected).await;
                    info!(target: LOG_GATEWAY, "Established lightning payment stream");

                    let route_payments_response =
                        self_copy.route_lightning_payments(&handle, stream, ln_client).await;

                    self_copy.set_gateway_state(GatewayState::Disconnected).await;
                    if let Err(err) = payment_stream_task_group.shutdown_join_all(None).await {
                        crit!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Lightning payment stream task group shutdown");
                    }

                    match route_payments_response {
                        ReceivePaymentStreamAction::RetryAfterDelay => {
                            warn!(target: LOG_GATEWAY, retry_interval = %PAYMENT_STREAM_RETRY_SECONDS, "Disconnected from lightning node");
                            sleep(Duration::from_secs(PAYMENT_STREAM_RETRY_SECONDS)).await;
                        }
                        ReceivePaymentStreamAction::NoRetry => break,
                    }
                }
            },
        );
    }

    /// Handles a stream of incoming payments from the lightning node after
    /// ensuring the gateway is properly configured. Awaits until the stream
    /// is closed, then returns with the appropriate action to take.
    async fn route_lightning_payments<'a>(
        &'a self,
        handle: &TaskHandle,
        mut stream: RouteHtlcStream<'a>,
        ln_client: Arc<GatewayLdkClient>,
    ) -> ReceivePaymentStreamAction {
        let LightningInfo::Connected {
            public_key: lightning_public_key,
            alias: lightning_alias,
            network: lightning_network,
            block_height: _,
            synced_to_chain,
        } = ln_client.parsed_node_info().await
        else {
            warn!(target: LOG_GATEWAY, "Failed to retrieve Lightning info");
            return ReceivePaymentStreamAction::RetryAfterDelay;
        };

        assert!(
            self.network == lightning_network,
            "Lightning node network does not match Gateway's network. LN: {lightning_network} Gateway: {}",
            self.network
        );

        if synced_to_chain || is_env_var_set(FM_GATEWAY_SKIP_WAIT_FOR_SYNC_ENV) {
            info!(target: LOG_GATEWAY, "Gateway is already synced to chain");
        } else {
            self.set_gateway_state(GatewayState::Syncing).await;
            info!(target: LOG_GATEWAY, "Waiting for chain sync");
            if let Err(err) = ln_client.wait_for_chain_sync().await {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to wait for chain sync");
                return ReceivePaymentStreamAction::RetryAfterDelay;
            }
        }

        let lightning_context = LightningContext {
            lnrpc: ln_client,
            lightning_public_key,
            lightning_alias,
            lightning_network,
        };
        self.set_gateway_state(GatewayState::Running { lightning_context })
            .await;
        info!(target: LOG_GATEWAY, "Gateway is running");

        // Runs until the connection to the lightning node breaks or we receive the
        // shutdown signal.
        let htlc_task_group = self.task_group.make_subgroup();
        if handle
            .cancel_on_shutdown(async move {
                loop {
                    let payment_request_or = tokio::select! {
                        payment_request_or = stream.next() => {
                            payment_request_or
                        }
                        () = self.is_shutting_down_safely() => {
                            break;
                        }
                    };

                    let Some(payment_request) = payment_request_or else {
                        warn!(
                            target: LOG_GATEWAY,
                            "Unexpected response from incoming lightning payment stream. Shutting down payment processor"
                        );
                        break;
                    };

                    let state_guard = self.state.read().await;
                    if let GatewayState::Running { ref lightning_context } = *state_guard {
                        // Spawn a subtask to handle each payment in parallel
                        let gateway = self.clone();
                        let lightning_context = lightning_context.clone();
                        htlc_task_group.spawn_cancellable_silent(
                            "handle_lightning_payment",
                            async move {
                                let start = fedimint_core::time::now();
                                let outcome = gateway
                                    .handle_lightning_payment(payment_request, &lightning_context)
                                    .await;
                                metrics::HTLC_HANDLING_DURATION_SECONDS
                                    .with_label_values(&[outcome])
                                    .observe(
                                        fedimint_core::time::now()
                                            .duration_since(start)
                                            .unwrap_or_default()
                                            .as_secs_f64(),
                                    );
                            },
                        );
                    } else {
                        warn!(
                            target: LOG_GATEWAY,
                            state = %state_guard,
                            "Gateway isn't in a running state, cannot handle incoming payments."
                        );
                        break;
                    }
                }
            })
            .await
            .is_ok()
        {
            warn!(target: LOG_GATEWAY, "Lightning payment stream connection broken. Gateway is disconnected");
            ReceivePaymentStreamAction::RetryAfterDelay
        } else {
            info!(target: LOG_GATEWAY, "Received shutdown signal");
            ReceivePaymentStreamAction::NoRetry
        }
    }

    /// Polls the Gateway's state waiting for it to shutdown so the thread
    /// processing payment requests can exit.
    async fn is_shutting_down_safely(&self) {
        loop {
            if let GatewayState::ShuttingDown { .. } = self.get_state().await {
                return;
            }

            fedimint_core::task::sleep(Duration::from_secs(1)).await;
        }
    }

    /// Handles an intercepted lightning payment. If the payment is part of an
    /// incoming payment to a federation, spawns a state machine and hands the
    /// payment off to it. If the payment's last-hop short channel id maps to
    /// a known federation but no LNv2 offer matched, cancels (fails back) the
    /// HTLC so the sender can retry rather than treating the gateway as a dead
    /// route. Otherwise (real-channel forwards), resumes the HTLC so the
    /// lightning node can route it as a normal forward.
    ///
    /// Returns the outcome label for metrics tracking.
    async fn handle_lightning_payment(
        &self,
        payment_request: InterceptPaymentRequest,
        lightning_context: &LightningContext,
    ) -> &'static str {
        info!(
            target: LOG_GATEWAY,
            lightning_payment = %PrettyInterceptPaymentRequest(&payment_request),
            "Intercepting lightning payment",
        );

        let lnv2_start = fedimint_core::time::now();
        let lnv2_result = self
            .try_handle_lightning_payment_lnv2(&payment_request, lightning_context)
            .await;
        let lnv2_outcome = if lnv2_result.is_ok() {
            "success"
        } else {
            "error"
        };
        metrics::HTLC_LNV2_ATTEMPT_DURATION_SECONDS
            .with_label_values(&[lnv2_outcome])
            .observe(
                fedimint_core::time::now()
                    .duration_since(lnv2_start)
                    .unwrap_or_default()
                    .as_secs_f64(),
            );
        if lnv2_result.is_ok() {
            return "lnv2";
        }

        // The LNv2 attempt did not match, so this HTLC is not an incoming
        // payment for one of our federations. The gateway's interceptor sees
        // every HTLC; resume so the lightning node forwards it as a regular
        // routing node.
        Self::forward_lightning_payment(payment_request, lightning_context).await;
        "forward"
    }

    /// Tries to handle a lightning payment using the LNv2 protocol.
    /// Returns `Ok` if the payment was handled, `Err` otherwise.
    async fn try_handle_lightning_payment_lnv2(
        &self,
        htlc_request: &InterceptPaymentRequest,
        lightning_context: &LightningContext,
    ) -> Result<()> {
        // If `payment_hash` has been registered as a LNv2 payment, we try to complete
        // the payment by getting the preimage from the federation
        // using the LNv2 protocol. If the `payment_hash` is not registered,
        // this payment is either a legacy Lightning payment or the end destination is
        // not a Fedimint.
        let (contract, client) = self
            .get_registered_incoming_contract_and_client_v2(
                PaymentImage::Hash(htlc_request.payment_hash),
                htlc_request.amount_msat,
            )
            .await?;

        if let Err(err) = client
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .relay_incoming_htlc(
                htlc_request.payment_hash,
                htlc_request.incoming_chan_id,
                htlc_request.htlc_id,
                contract,
                htlc_request.amount_msat,
            )
            .await
        {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Error relaying incoming lightning payment");

            let outcome = InterceptPaymentResponse {
                action: PaymentAction::Cancel,
                payment_hash: htlc_request.payment_hash,
                incoming_chan_id: htlc_request.incoming_chan_id,
                htlc_id: htlc_request.htlc_id,
            };

            if let Err(err) = lightning_context.lnrpc.complete_htlc(outcome).await {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Error sending HTLC response to lightning node");
            }
        }

        Ok(())
    }

    /// Forwards a lightning payment to the next hop like a normal lightning
    /// node. Used when the intercepted HTLC is not destined for any federation
    /// this gateway serves, so LND should route it normally over a real
    /// channel.
    async fn forward_lightning_payment(
        htlc_request: InterceptPaymentRequest,
        lightning_context: &LightningContext,
    ) {
        let outcome = InterceptPaymentResponse {
            action: PaymentAction::Forward,
            payment_hash: htlc_request.payment_hash,
            incoming_chan_id: htlc_request.incoming_chan_id,
            htlc_id: htlc_request.htlc_id,
        };

        if let Err(err) = lightning_context.lnrpc.complete_htlc(outcome).await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Error sending lightning payment response to lightning node");
        }
    }

    /// Helper function for atomically changing the Gateway's internal state.
    async fn set_gateway_state(&self, state: GatewayState) {
        let mut lock = self.state.write().await;
        *lock = state;
    }

    /// If the Gateway is connected to the Lightning node, returns the
    /// `ClientConfig` for each federation that the Gateway is connected to.
    pub async fn handle_get_federation_config(
        &self,
        federation_id_or: Option<FederationId>,
    ) -> AdminResult<GatewayFedConfig> {
        if !matches!(self.get_state().await, GatewayState::Running { .. }) {
            return Ok(GatewayFedConfig {
                federations: BTreeMap::new(),
            });
        }

        let federations = if let Some(federation_id) = federation_id_or {
            let mut federations = BTreeMap::new();
            federations.insert(
                federation_id,
                self.federation_manager
                    .read()
                    .await
                    .get_federation_config(federation_id)
                    .await?,
            );
            federations
        } else {
            self.federation_manager
                .read()
                .await
                .get_all_federation_configs()
                .await
        };

        Ok(GatewayFedConfig { federations })
    }

    /// Returns a Bitcoin deposit on-chain address for pegging in Bitcoin for a
    /// specific connected federation.
    pub async fn handle_address_msg(&self, payload: DepositAddressPayload) -> AdminResult<Address> {
        let client = self.select_client(payload.federation_id).await?;

        if let Ok(wallet_module) = client
            .value()
            .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        {
            Ok(wallet_module.receive().await)
        } else {
            Err(AdminGatewayError::Unexpected(anyhow!(
                "No wallet module found"
            )))
        }
    }

    /// Handles a request for the gateway to backup a connected federation's
    /// ecash.
    pub async fn handle_backup_msg(
        &self,
        BackupPayload { federation_id }: BackupPayload,
    ) -> AdminResult<()> {
        let federation_manager = self.federation_manager.read().await;
        let client = federation_manager
            .client(&federation_id)
            .ok_or(AdminGatewayError::ClientCreationError(anyhow::anyhow!(
                format!("Gateway has not connected to {federation_id}")
            )))?
            .value();
        let metadata: BTreeMap<String, String> = BTreeMap::new();
        #[allow(deprecated)]
        client
            .backup_to_federation(fedimint_client::backup::Metadata::from_json_serialized(
                metadata,
            ))
            .await?;
        Ok(())
    }

    /// Trigger rechecking for deposits on an address
    pub async fn handle_recheck_address_msg(
        &self,
        payload: DepositAddressRecheckPayload,
    ) -> AdminResult<()> {
        let client = self.select_client(payload.federation_id).await?;

        if client
            .value()
            .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
            .is_ok()
        {
            // Walletv2 auto-claims deposits, so this is a no-op
            Ok(())
        } else {
            Err(AdminGatewayError::Unexpected(anyhow!(
                "No wallet module found"
            )))
        }
    }

    /// Handles a request to receive ecash into the gateway.
    pub async fn handle_receive_ecash_msg(
        &self,
        payload: ReceiveEcashPayload,
    ) -> Result<ReceiveEcashResponse> {
        let federation_id_prefix = base32::decode_prefixed::<fedimint_mintv2_client::ECash>(
            FEDIMINT_PREFIX,
            &payload.notes,
        )
        .ok()
        .and_then(|e| e.mint())
        .map(|id| id.to_prefix())
        .ok_or_else(|| PublicGatewayError::ReceiveEcashError {
            failure_reason: "Invalid ecash format: could not parse as ECash".to_string(),
        })?;

        let client = self
            .federation_manager
            .read()
            .await
            .get_client_for_federation_id_prefix(federation_id_prefix)
            .ok_or(FederationNotConnected {
                federation_id_prefix,
            })?;

        // Check which module is present and parse accordingly
        if let Ok(mint) = client.value().get_first_module::<MintV2ClientModule>() {
            let ecash: fedimint_mintv2_client::ECash =
                base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes).map_err(|e| {
                    PublicGatewayError::ReceiveEcashError {
                        failure_reason: format!("Expected ECash for MintV2 federation: {e}"),
                    }
                })?;
            let amount = ecash.amount();

            let operation_id = mint
                .receive(ecash, serde_json::Value::Null)
                .await
                .map_err(|e| PublicGatewayError::ReceiveEcashError {
                    failure_reason: e.to_string(),
                })?;

            if payload.wait {
                let final_state = mint
                    .await_final_receive_operation_state(operation_id)
                    .await
                    .map_err(|e| PublicGatewayError::ReceiveEcashError {
                        failure_reason: e.to_string(),
                    })?;
                match final_state {
                    fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
                    fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
                        return Err(PublicGatewayError::ReceiveEcashError {
                            failure_reason: "ECash receive was rejected".to_string(),
                        });
                    }
                }
            }

            Ok(ReceiveEcashResponse { amount })
        } else {
            Err(PublicGatewayError::ReceiveEcashError {
                failure_reason: "No mint module found".to_string(),
            })
        }
    }

    /// Retrieves an invoice by the payment hash if it exists, otherwise returns
    /// `None`.
    pub async fn handle_get_invoice_msg(
        &self,
        payload: GetInvoiceRequest,
    ) -> AdminResult<Option<GetInvoiceResponse>> {
        let lightning_context = self.get_lightning_context().await?;
        let invoice = lightning_context.lnrpc.get_invoice(payload).await?;
        Ok(invoice)
    }

    /// Withdraws ecash from a federation and pegs-out to the Lightning node's
    /// onchain wallet
    pub async fn handle_withdraw_to_onchain_msg(
        &self,
        payload: WithdrawToOnchainPayload,
    ) -> AdminResult<WithdrawResponse> {
        let address = self.handle_get_ln_onchain_address_msg().await?;
        let withdraw = WithdrawPayload {
            address: address.into_unchecked(),
            federation_id: payload.federation_id,
            amount: payload.amount,
            quoted_fees: None,
        };
        self.handle_withdraw_msg(withdraw).await
    }

    /// Deposits the specified amount from the gateway's onchain wallet into the
    /// Federation's ecash wallet
    pub async fn handle_pegin_from_onchain_msg(
        &self,
        payload: PeginFromOnchainPayload,
    ) -> AdminResult<Txid> {
        let deposit = DepositAddressPayload {
            federation_id: payload.federation_id,
        };
        let address = self.handle_address_msg(deposit).await?;
        let send_onchain = SendOnchainRequest {
            address: address.into_unchecked(),
            amount: payload.amount,
            fee_rate_sats_per_vbyte: payload.fee_rate_sats_per_vbyte,
        };
        let txid = self.handle_send_onchain_msg(send_onchain).await?;

        Ok(txid)
    }

    /// Retrieves a `ClientHandleArc` from the Gateway's in memory structures
    /// that keep track of available clients, given a `federation_id`.
    pub async fn select_client(
        &self,
        federation_id: FederationId,
    ) -> std::result::Result<Spanned<fedimint_client::ClientHandleArc>, FederationNotConnected>
    {
        self.federation_manager
            .read()
            .await
            .client(&federation_id)
            .cloned()
            .ok_or(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            })
    }

    async fn load_mnemonic(gateway_db: &Database) -> Option<Mnemonic> {
        let entropy = gateway_db
            .begin_transaction_nc()
            .await
            .load_root_entropy()
            .await?;
        Mnemonic::from_entropy(&entropy).ok()
    }

    /// Reads the connected federation client configs from the Gateway's
    /// database and reconstructs the clients necessary for interacting with
    /// connection federations.
    async fn load_clients(&self) -> AdminResult<()> {
        let mut federation_manager = self.federation_manager.write().await;

        let federation_ids = {
            let mut dbtx = self.gateway_db.begin_transaction_nc().await;
            dbtx.load_client_configs()
                .await
                .into_keys()
                .collect::<Vec<_>>()
        };

        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");

        for federation_id in federation_ids {
            match Box::pin(Spanned::try_new(
                info_span!(target: LOG_GATEWAY, "client", federation_id  = %federation_id.clone()),
                self.client_builder
                    .build(federation_id, None, Arc::new(self.clone()), &mnemonic),
            ))
            .await
            {
                Ok(client) => {
                    federation_manager.add_client(client);
                }
                _ => {
                    warn!(target: LOG_GATEWAY, federation_id = %federation_id, "Failed to load client");
                }
            }
        }

        Ok(())
    }

    /// Verifies that the federation has an LNv2 lightning module and that the
    /// network matches the gateway's network.
    async fn check_federation_network(
        client: &ClientHandleArc,
        network: Network,
    ) -> AdminResult<()> {
        let federation_id = client.federation_id();
        let config = client.config().await;

        let lnv2_cfg = config
            .modules
            .values()
            .find(|m| fedimint_lnv2_common::LightningCommonInit::KIND == m.kind);

        // Ensure the federation has an LNv2 lightning module
        let Some(cfg) = lnv2_cfg else {
            return Err(AdminGatewayError::ClientCreationError(anyhow!(
                "Federation {federation_id} does not have an LNv2 lightning module"
            )));
        };

        // Verify the LNv2 network
        let ln_cfg: &fedimint_lnv2_common::config::LightningClientConfig = cfg.cast()?;

        if ln_cfg.network != network {
            crit!(
                target: LOG_GATEWAY,
                federation_id = %federation_id,
                network = %network,
                "Incorrect LNv2 network for federation",
            );
            return Err(AdminGatewayError::ClientCreationError(anyhow!(format!(
                "Unsupported LNv2 network {}",
                ln_cfg.network
            ))));
        }

        Ok(())
    }

    /// Checks the Gateway's current state and returns the proper
    /// `LightningContext` if it is available. Sometimes the lightning node
    /// will not be connected and this will return an error.
    pub async fn get_lightning_context(
        &self,
    ) -> std::result::Result<LightningContext, LightningRpcError> {
        match self.get_state().await {
            GatewayState::Running { lightning_context }
            | GatewayState::ShuttingDown { lightning_context } => Ok(lightning_context),
            _ => Err(LightningRpcError::FailedToConnect),
        }
    }

    async fn create_lightning_client(
        &self,
        runtime: Arc<tokio::runtime::Runtime>,
    ) -> Box<GatewayLdkClient> {
        match self.lightning_mode.clone() {
            LightningMode::Lnd { .. } => {
                panic!("gatewaydv2 only supports the LDK lightning backend, not LND")
            }
            LightningMode::Ldk {
                lightning_port,
                alias,
            } => {
                let mnemonic = Self::load_mnemonic(&self.gateway_db)
                    .await
                    .expect("mnemonic should be set");
                // Retrieving the fees inside of LDK can sometimes fail/time out. To prevent
                // crashing the gateway, we wait a bit and just try
                // to re-create the client. The gateway cannot proceed until this succeeds.
                retry("create LDK Node", fibonacci_max_one_hour(), || async {
                    GatewayLdkClient::new(
                        &self.client_builder.data_dir().join(LDK_NODE_DB_FOLDER),
                        self.chain_source.clone(),
                        self.network,
                        lightning_port,
                        alias.clone(),
                        mnemonic.clone(),
                        runtime.clone(),
                    )
                    .map(Box::new)
                })
                .await
                .expect("Could not create LDK Node")
            }
        }
    }
}

/// Admin/operator request handlers, called by the REST API. (`gatewaydv2` has
/// no web UI, so these are plain inherent methods rather than an
/// `IAdminGateway` trait impl.)
impl Gateway {
    /// Returns information about the Gateway back to the client when requested
    /// via the webserver.
    async fn handle_get_info(&self) -> AdminResult<GatewayInfo> {
        let GatewayState::Running { lightning_context } = self.get_state().await else {
            return Ok(GatewayInfo {
                federations: vec![],
                federation_fake_scids: None,
                version_hash: fedimint_build_code_version_env!().to_string(),
                gateway_state: self.state.read().await.to_string(),
                lightning_info: LightningInfo::NotConnected,
                lightning_mode: self.lightning_mode.clone(),
                registrations: self.registrations_with_id().await,
            });
        };

        let federations = self
            .federation_manager
            .read()
            .await
            .federation_info_all_federations(
                self.default_routing_fees,
                self.default_transaction_fees,
            )
            .await;

        let lightning_info = lightning_context.lnrpc.parsed_node_info().await;

        Ok(GatewayInfo {
            federations,
            // `federation_fake_scids` is a vestigial LNv1/LND-interceptor concept.
            federation_fake_scids: None,
            version_hash: fedimint_build_code_version_env!().to_string(),
            gateway_state: self.state.read().await.to_string(),
            lightning_info,
            lightning_mode: self.lightning_mode.clone(),
            registrations: self.registrations_with_id().await,
        })
    }

    /// Builds the advertised registration map, pairing each protocol's endpoint
    /// url with the gateway's mnemonic-derived identity public key. Empty if
    /// the mnemonic has not been set yet.
    async fn registrations_with_id(&self) -> BTreeMap<RegisteredProtocol, (SafeUrl, PublicKey)> {
        let Some(gateway_id) = self.gateway_id().await else {
            return BTreeMap::new();
        };
        self.registrations
            .iter()
            .map(|(protocol, registration)| {
                (
                    protocol.clone(),
                    (registration.endpoint_url.clone(), gateway_id),
                )
            })
            .collect()
    }

    /// Returns a list of Lightning network channels from the Gateway's
    /// Lightning node.
    async fn handle_list_channels_msg(
        &self,
    ) -> AdminResult<Vec<fedimint_gateway_common::ChannelInfo>> {
        let context = self.get_lightning_context().await?;
        let response = context.lnrpc.list_channels().await?;
        Ok(response.channels)
    }

    /// Computes the 24 hour payment summary statistics for this gateway.
    async fn handle_payment_summary_msg(
        &self,
        PaymentSummaryPayload {
            start_millis,
            end_millis,
        }: PaymentSummaryPayload,
    ) -> AdminResult<PaymentSummaryResponse> {
        let federation_manager = self.federation_manager.read().await;
        let fed_configs = federation_manager.get_all_federation_configs().await;
        let federation_ids = fed_configs.keys().collect::<Vec<_>>();
        let start = UNIX_EPOCH + Duration::from_millis(start_millis);
        let end = UNIX_EPOCH + Duration::from_millis(end_millis);

        if start > end {
            return Err(AdminGatewayError::Unexpected(anyhow!("Invalid time range")));
        }

        let mut outgoing = StructuredPaymentEvents::default();
        let mut incoming = StructuredPaymentEvents::default();
        for fed_id in federation_ids {
            let client = federation_manager
                .client(fed_id)
                .expect("No client available")
                .value();
            let all_events = &get_events_for_duration(client, start, end).await;

            let (mut lnv2_outgoing, mut lnv2_incoming) = compute_lnv2_stats(all_events);
            outgoing.combine(&mut lnv2_outgoing);
            incoming.combine(&mut lnv2_incoming);
        }

        Ok(PaymentSummaryResponse {
            outgoing: PaymentStats::compute(&outgoing),
            incoming: PaymentStats::compute(&incoming),
        })
    }

    /// Handle a request to have the Gateway leave a federation. The Gateway
    /// will request the federation to remove the registration record and
    /// the gateway will remove the configuration needed to construct the
    /// federation client.
    async fn handle_leave_federation(
        &self,
        payload: LeaveFedPayload,
    ) -> AdminResult<FederationInfo> {
        // Lock the federation manager before starting the db transaction to reduce the
        // chance of db write conflicts.
        let mut federation_manager = self.federation_manager.write().await;

        let federation_info = federation_manager
            .leave_federation(
                payload.federation_id,
                self.default_routing_fees,
                self.default_transaction_fees,
            )
            .await?;

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.remove_client_config(payload.federation_id).await;
        dbtx.commit_tx().await;
        Ok(federation_info)
    }

    /// Handles a connection request to join a new federation. The gateway will
    /// download the federation's client configuration, construct a new
    /// client, registers, the gateway with the federation, and persists the
    /// necessary config to reconstruct the client when restarting the gateway.
    async fn handle_connect_federation(
        &self,
        payload: ConnectFedPayload,
    ) -> AdminResult<FederationInfo> {
        let GatewayState::Running { .. } = self.get_state().await else {
            return Err(AdminGatewayError::Lightning(
                LightningRpcError::FailedToConnect,
            ));
        };

        let invite_code = InviteCode::from_str(&payload.invite_code).map_err(|e| {
            AdminGatewayError::ClientCreationError(anyhow!(format!(
                "Invalid federation member string {e:?}"
            )))
        })?;

        let federation_id = invite_code.federation_id();

        let mut federation_manager = self.federation_manager.write().await;

        // Check if this federation has already been registered
        if federation_manager.has_federation(federation_id) {
            return Err(AdminGatewayError::ClientCreationError(anyhow!(
                "Federation has already been registered"
            )));
        }

        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");
        let recover = payload.recover.unwrap_or(false);
        if recover {
            self.client_builder
                .recover(&invite_code, Arc::new(self.clone()), &mnemonic)
                .await?;
        }

        let client = self
            .client_builder
            .build(
                federation_id,
                Some(&invite_code),
                Arc::new(self.clone()),
                &mnemonic,
            )
            .await?;

        if recover {
            client.wait_for_all_active_state_machines().await?;
        }

        let client_config = client.config().await;

        // Instead of using `FederationManager::federation_info`, we build the
        // federation info here from the freshly joined client.
        let federation_info = FederationInfo {
            federation_id,
            federation_name: federation_manager.federation_name(&client).await,
            balance_msat: client.get_balance_for_btc().await.unwrap_or_else(|err| {
                warn!(
                    target: LOG_GATEWAY,
                    err = %err.fmt_compact_anyhow(),
                    %federation_id,
                    "Balance not immediately available after joining/recovering."
                );
                Amount::default()
            }),
            config: FederationConfig {
                invite_code,
                federation_index: 0,
                lightning_fee: self.default_routing_fees,
                transaction_fee: self.default_transaction_fees,
                #[allow(deprecated)]
                _connector: ConnectorType::Tcp,
            },
            last_backup_time: None,
        };

        Self::check_federation_network(&client, self.network).await?;

        // no need to enter span earlier, because connect-fed has a span
        federation_manager.add_client(
            Spanned::new(
                info_span!(target: LOG_GATEWAY, "client", federation_id=%federation_id.clone()),
                async { client },
            )
            .await,
        );

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.save_client_config(&federation_id, &client_config)
            .await;
        dbtx.commit_tx().await;
        debug!(
            target: LOG_GATEWAY,
            federation_id = %federation_id,
            "Federation connected"
        );

        Ok(federation_info)
    }

    /// Handles an authenticated request for the gateway's mnemonic. This also
    /// returns a vector of federations that are not using the mnemonic
    /// backup strategy.
    async fn handle_mnemonic_msg(&self) -> AdminResult<MnemonicResponse> {
        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");
        let words = mnemonic
            .words()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        let all_federations = self
            .federation_manager
            .read()
            .await
            .get_all_federation_configs()
            .await
            .keys()
            .copied()
            .collect::<BTreeSet<_>>();
        let legacy_federations = self.client_builder.legacy_federations(all_federations);
        let mnemonic_response = MnemonicResponse {
            mnemonic: words,
            legacy_federations,
        };
        Ok(mnemonic_response)
    }

    /// Instructs the Gateway's Lightning node to open a channel to a peer
    /// specified by `pubkey`.
    async fn handle_open_channel_msg(&self, payload: OpenChannelRequest) -> AdminResult<Txid> {
        info!(target: LOG_GATEWAY, pubkey = %payload.pubkey, host = %payload.host, amount = %payload.channel_size_sats, "Opening Lightning channel...");
        let context = self.get_lightning_context().await?;
        let res = context.lnrpc.open_channel(payload).await?;
        info!(target: LOG_GATEWAY, txid = %res.funding_txid, "Initiated channel open");
        Txid::from_str(&res.funding_txid).map_err(|e| {
            AdminGatewayError::Lightning(LightningRpcError::InvalidMetadata {
                failure_reason: format!("Received invalid channel funding txid string {e}"),
            })
        })
    }

    /// Instructs the Gateway's Lightning node to close all channels with a peer
    /// specified by `pubkey`.
    async fn handle_close_channels_with_peer_msg(
        &self,
        payload: CloseChannelsWithPeerRequest,
    ) -> AdminResult<CloseChannelsWithPeerResponse> {
        info!(target: LOG_GATEWAY, close_channel_request = %payload, "Closing lightning channel...");
        let context = self.get_lightning_context().await?;
        let response = context
            .lnrpc
            .close_channels_with_peer(payload.clone())
            .await?;
        info!(target: LOG_GATEWAY, close_channel_request = %payload, "Initiated channel closure");
        Ok(response)
    }

    /// Updates the local-side routing fees (base + ppm) on a single channel
    /// identified by funding outpoint.
    async fn handle_set_channel_fees_msg(&self, payload: SetChannelFeesRequest) -> AdminResult<()> {
        info!(
            target: LOG_GATEWAY,
            funding_outpoint = %payload.funding_outpoint,
            base_fee_msat = payload.base_fee_msat,
            parts_per_million = payload.parts_per_million,
            "Updating channel fees..."
        );
        let context = self.get_lightning_context().await?;
        context.lnrpc.set_channel_fees(payload).await?;
        Ok(())
    }

    /// Returns the ecash, lightning, and onchain balances for the gateway and
    /// the gateway's lightning node.
    async fn handle_get_balances_msg(&self) -> AdminResult<GatewayBalances> {
        let federation_infos = self
            .federation_manager
            .read()
            .await
            .federation_info_all_federations(
                self.default_routing_fees,
                self.default_transaction_fees,
            )
            .await;

        let ecash_balances: Vec<FederationBalanceInfo> = federation_infos
            .iter()
            .map(|federation_info| FederationBalanceInfo {
                federation_id: federation_info.federation_id,
                ecash_balance_msats: Amount {
                    msats: federation_info.balance_msat.msats,
                },
            })
            .collect();

        let context = self.get_lightning_context().await?;
        let lightning_node_balances = context.lnrpc.get_balances().await?;

        Ok(GatewayBalances {
            onchain_balance_sats: lightning_node_balances.onchain_balance_sats,
            lightning_balance_msats: lightning_node_balances.lightning_balance_msats,
            ecash_balances,
            inbound_lightning_liquidity_msats: lightning_node_balances
                .inbound_lightning_liquidity_msats,
        })
    }

    /// Send funds from the gateway's lightning node on-chain wallet.
    async fn handle_send_onchain_msg(&self, payload: SendOnchainRequest) -> AdminResult<Txid> {
        let context = self.get_lightning_context().await?;
        let response = context.lnrpc.send_onchain(payload.clone()).await?;
        let txid =
            Txid::from_str(&response.txid).map_err(|e| AdminGatewayError::WithdrawError {
                failure_reason: format!("Failed to parse withdrawal TXID: {e}"),
            })?;
        info!(onchain_request = %payload, txid = %txid, "Sent onchain transaction");
        Ok(txid)
    }

    /// Generates an onchain address to fund the gateway's lightning node.
    async fn handle_get_ln_onchain_address_msg(&self) -> AdminResult<Address> {
        let context = self.get_lightning_context().await?;
        let response = context.lnrpc.get_ln_onchain_address().await?;

        let address = Address::from_str(&response.address).map_err(|e| {
            AdminGatewayError::Lightning(LightningRpcError::InvalidMetadata {
                failure_reason: e.to_string(),
            })
        })?;

        address.require_network(self.network).map_err(|e| {
            AdminGatewayError::Lightning(LightningRpcError::InvalidMetadata {
                failure_reason: e.to_string(),
            })
        })
    }

    /// Creates an invoice that is directly payable to the gateway's lightning
    /// node.
    async fn handle_create_invoice_for_operator_msg(
        &self,
        payload: CreateInvoiceForOperatorPayload,
    ) -> AdminResult<Bolt11Invoice> {
        let GatewayState::Running { lightning_context } = self.get_state().await else {
            return Err(AdminGatewayError::Lightning(
                LightningRpcError::FailedToConnect,
            ));
        };

        Bolt11Invoice::from_str(
            &lightning_context
                .lnrpc
                .create_invoice(CreateInvoiceRequest {
                    payment_hash: None, /* Empty payment hash indicates an invoice payable
                                         * directly to the gateway. */
                    amount_msat: payload.amount_msats,
                    expiry_secs: payload.expiry_secs.unwrap_or(3600),
                    description: payload.description.map(InvoiceDescription::Direct),
                })
                .await?
                .invoice,
        )
        .map_err(|e| {
            AdminGatewayError::Lightning(LightningRpcError::InvalidMetadata {
                failure_reason: e.to_string(),
            })
        })
    }

    /// Requests the gateway to pay an outgoing LN invoice using its own funds.
    /// Returns the payment hash's preimage on success.
    async fn handle_pay_invoice_for_operator_msg(
        &self,
        payload: PayInvoiceForOperatorPayload,
    ) -> AdminResult<Preimage> {
        // Those are the ldk defaults
        const BASE_FEE: u64 = 50;
        const FEE_DENOMINATOR: u64 = 100;
        const MAX_DELAY: u64 = 1008;

        let GatewayState::Running { lightning_context } = self.get_state().await else {
            return Err(AdminGatewayError::Lightning(
                LightningRpcError::FailedToConnect,
            ));
        };

        let max_fee = BASE_FEE
            + payload
                .invoice
                .amount_milli_satoshis()
                .context("Invoice is missing amount")?
                .saturating_div(FEE_DENOMINATOR);

        let res = lightning_context
            .lnrpc
            .pay(payload.invoice, MAX_DELAY, Amount::from_msats(max_fee))
            .await?;
        Ok(res.preimage)
    }

    /// Lists the transactions that the lightning node has made.
    async fn handle_list_transactions_msg(
        &self,
        payload: ListTransactionsPayload,
    ) -> AdminResult<ListTransactionsResponse> {
        let lightning_context = self.get_lightning_context().await?;
        let response = lightning_context
            .lnrpc
            .list_transactions(payload.start_secs, payload.end_secs)
            .await?;
        Ok(response)
    }

    // Handles a request the spend the gateway's ecash for a given federation.
    async fn handle_spend_ecash_msg(
        &self,
        payload: SpendEcashPayload,
    ) -> AdminResult<SpendEcashResponse> {
        let client = self
            .select_client(payload.federation_id)
            .await?
            .into_value();

        if let Ok(mint_module) = client.get_first_module::<MintV2ClientModule>() {
            let (_, ecash) = mint_module
                .send(payload.amount, serde_json::Value::Null, true)
                .await
                .map_err(|e| AdminGatewayError::Unexpected(e.into()))?;

            Ok(SpendEcashResponse {
                notes: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
            })
        } else {
            Err(AdminGatewayError::Unexpected(anyhow::anyhow!(
                "No mint module available"
            )))
        }
    }

    /// Instructs the gateway to shutdown, but only after all incoming payments
    /// have been handled.
    async fn handle_shutdown_msg(&self, task_group: TaskGroup) -> AdminResult<()> {
        // Take the write lock on the state so that no additional payments are processed
        let mut state_guard = self.state.write().await;
        if let GatewayState::Running { lightning_context } = state_guard.clone() {
            *state_guard = GatewayState::ShuttingDown { lightning_context };

            self.federation_manager
                .read()
                .await
                .wait_for_incoming_payments()
                .await?;
        }

        let tg = task_group.clone();
        tg.spawn("Kill Gateway", |_task_handle| async {
            if let Err(err) = task_group.shutdown_join_all(Duration::from_mins(3)).await {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Error shutting down gateway");
            }
        });
        Ok(())
    }

    /// Returns a Bitcoin TXID from a peg-out transaction for a specific
    /// connected federation.
    async fn handle_withdraw_msg(&self, payload: WithdrawPayload) -> AdminResult<WithdrawResponse> {
        let WithdrawPayload {
            amount,
            address,
            federation_id,
            quoted_fees: _,
        } = payload;

        let address_network = get_network_for_address(&address);
        let gateway_network = self.network;
        let Ok(address) = address.require_network(gateway_network) else {
            return Err(AdminGatewayError::WithdrawError {
                failure_reason: format!(
                    "Gateway is running on network {gateway_network}, but provided withdraw address is for network {address_network}"
                ),
            });
        };

        let client = self.select_client(federation_id).await?;

        let wallet_module = client
            .value()
            .get_first_module::<fedimint_walletv2_client::WalletClientModule>()?;

        withdraw_v2(client.value(), &wallet_module, &address, amount).await
    }

    /// Queries the client log for payment events and returns to the user.
    /// Returns a paginated list of gateway payment-related events, ordered from
    /// newest to oldest.
    ///
    /// If `event_kinds` is empty, only events matching `ALL_GATEWAY_EVENTS`
    /// are returned — this is **not** equivalent to "all events". Other
    /// internal events (e.g. `tx-created`, `NoteCreated`) share the same event
    /// log and consume IDs, so returned event IDs may be non-contiguous.
    ///
    /// Pagination works backwards from `end_position` (or the log tip if
    /// `None`), returning at most `pagination_size` matching events.
    async fn handle_payment_log_msg(
        &self,
        PaymentLogPayload {
            end_position,
            pagination_size,
            federation_id,
            event_kinds,
        }: PaymentLogPayload,
    ) -> AdminResult<PaymentLogResponse> {
        const BATCH_SIZE: u64 = 10_000;
        let federation_manager = self.federation_manager.read().await;
        let client = federation_manager
            .client(&federation_id)
            .ok_or(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            })?
            .value();

        // An empty `event_kinds` defaults to gateway payment-related events, not
        // "all events". This means returned event IDs may be non-contiguous since
        // other internal events share the same ID space.
        let event_kinds = if event_kinds.is_empty() {
            ALL_GATEWAY_EVENTS.to_vec()
        } else {
            event_kinds
        };

        let end_position = if let Some(position) = end_position {
            position
        } else {
            let mut dbtx = client.db().begin_transaction_nc().await;
            dbtx.get_next_event_log_id().await
        };

        let mut start_position = end_position.saturating_sub(BATCH_SIZE);

        let mut payment_log = Vec::new();

        while payment_log.len() < pagination_size {
            let batch = client.get_event_log(Some(start_position), BATCH_SIZE).await;
            let mut filtered_batch = batch
                .into_iter()
                .filter(|e| e.id() <= end_position && event_kinds.contains(&e.as_raw().kind))
                .collect::<Vec<_>>();
            filtered_batch.reverse();
            payment_log.extend(filtered_batch);

            // Compute the start position for the next batch query
            start_position = start_position.saturating_sub(BATCH_SIZE);

            if start_position == EventLogId::LOG_START {
                break;
            }
        }

        // Truncate the payment log to the expected pagination size
        payment_log.truncate(pagination_size);

        Ok(PaymentLogResponse(payment_log))
    }

    /// Creates a BOLT12 offer using the gateway's lightning node
    async fn handle_create_offer_for_operator_msg(
        &self,
        payload: CreateOfferPayload,
    ) -> AdminResult<CreateOfferResponse> {
        let lightning_context = self.get_lightning_context().await?;
        let offer = lightning_context.lnrpc.create_offer(
            payload.amount,
            payload.description,
            payload.expiry_secs,
            payload.quantity,
        )?;
        Ok(CreateOfferResponse { offer })
    }

    /// Pays a BOLT12 offer using the gateway's lightning node
    async fn handle_pay_offer_for_operator_msg(
        &self,
        payload: PayOfferPayload,
    ) -> AdminResult<PayOfferResponse> {
        let lightning_context = self.get_lightning_context().await?;
        let preimage = lightning_context
            .lnrpc
            .pay_offer(
                payload.offer,
                payload.quantity,
                payload.amount,
                payload.payer_note,
            )
            .await?;
        Ok(PayOfferResponse {
            preimage: preimage.to_string(),
        })
    }

    /// Returns a `BTreeMap` that is keyed by the `FederationId` and contains
    /// all the invite codes (with peer names) for the federation.
    async fn handle_export_invite_codes(
        &self,
    ) -> BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>> {
        let fed_manager = self.federation_manager.read().await;
        fed_manager.all_invite_codes().await
    }
}

// LNv2 Gateway implementation
impl Gateway {
    /// Retrieves the `PublicKey` of the Gateway module for a given federation
    /// for LNv2. This is NOT the same as the `gateway_id`, it is different
    /// per-connected federation.
    async fn public_key_v2(&self, federation_id: &FederationId) -> Option<PublicKey> {
        self.federation_manager
            .read()
            .await
            .client(federation_id)
            .map(|client| {
                client
                    .value()
                    .get_first_module::<GatewayClientModuleV2>()
                    .expect("Must have client module")
                    .keypair
                    .public_key()
            })
    }

    /// Returns payment information that LNv2 clients can use to instruct this
    /// Gateway to pay an invoice or receive a payment.
    pub async fn routing_info_v2(
        &self,
        federation_id: &FederationId,
    ) -> Result<Option<RoutingInfo>> {
        let context = self.get_lightning_context().await?;

        if !self
            .federation_manager
            .read()
            .await
            .has_federation(*federation_id)
        {
            return Err(PublicGatewayError::FederationNotConnected(
                FederationNotConnected {
                    federation_id_prefix: federation_id.to_prefix(),
                },
            ));
        }

        let lightning_fee = self.default_routing_fees;
        let transaction_fee = self.default_transaction_fees;

        Ok(self
            .public_key_v2(federation_id)
            .await
            .map(|module_public_key| RoutingInfo {
                lightning_public_key: context.lightning_public_key,
                lightning_alias: Some(context.lightning_alias.clone()),
                module_public_key,
                send_fee_default: lightning_fee + transaction_fee,
                // The base fee ensures that the gateway does not loose sats sending the payment due
                // to fees paid on the transaction claiming the outgoing contract or
                // subsequent transactions spending the newly issued ecash
                send_fee_minimum: transaction_fee,
                expiration_delta_default: 1440,
                expiration_delta_minimum: EXPIRATION_DELTA_MINIMUM_V2,
                // The base fee ensures that the gateway does not loose sats receiving the payment
                // due to fees paid on the transaction funding the incoming contract
                receive_fee: transaction_fee,
            }))
    }

    /// Instructs this gateway to pay a Lightning network invoice via the LNv2
    /// protocol.
    async fn send_payment_v2(
        &self,
        payload: SendPaymentPayload,
    ) -> Result<std::result::Result<[u8; 32], Signature>> {
        self.select_client(payload.federation_id)
            .await?
            .value()
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .send_payment(payload)
            .await
            .map_err(LNv2Error::OutgoingPayment)
            .map_err(PublicGatewayError::LNv2)
    }

    /// For the LNv2 protocol, this will create an invoice by fetching it from
    /// the connected Lightning node, then save the payment hash so that
    /// incoming lightning payments can be matched as a receive attempt to a
    /// specific federation.
    async fn create_bolt11_invoice_v2(
        &self,
        payload: CreateBolt11InvoicePayload,
    ) -> Result<Bolt11Invoice> {
        if !payload.contract.verify() {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "The contract is invalid".to_string(),
            )));
        }

        let payment_info = self.routing_info_v2(&payload.federation_id).await?.ok_or(
            LNv2Error::IncomingPayment(format!(
                "Federation {} does not exist",
                payload.federation_id
            )),
        )?;

        if payload.contract.commitment.refund_pk != payment_info.module_public_key {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "The incoming contract is keyed to another gateway".to_string(),
            )));
        }

        let contract_amount = payment_info.receive_fee.subtract_from(payload.amount.msats);

        if contract_amount == Amount::ZERO {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "Zero amount incoming contracts are not supported".to_string(),
            )));
        }

        if contract_amount != payload.contract.commitment.amount {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "The contract amount does not pay the correct amount of fees".to_string(),
            )));
        }

        if payload.contract.commitment.expiration_or_fee <= duration_since_epoch().as_secs() {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "The contract has already expired".to_string(),
            )));
        }

        let payment_hash = match payload.contract.commitment.payment_image {
            PaymentImage::Hash(payment_hash) => payment_hash,
            PaymentImage::Point(..) => {
                return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                    "PaymentImage is not a payment hash".to_string(),
                )));
            }
        };

        let invoice = self
            .create_invoice_via_lnrpc_v2(
                payment_hash,
                payload.amount,
                payload.description.clone(),
                payload.expiry_secs,
            )
            .await?;

        let mut dbtx = self.gateway_db.begin_transaction().await;

        if dbtx
            .save_registered_incoming_contract(
                payload.federation_id,
                payload.amount,
                payload.contract,
            )
            .await
            .is_some()
        {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "PaymentHash is already registered".to_string(),
            )));
        }

        dbtx.commit_tx_result().await.map_err(|_| {
            PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "Payment hash is already registered".to_string(),
            ))
        })?;

        Ok(invoice)
    }

    /// Retrieves a BOLT11 invoice from the connected Lightning node with a
    /// specific `payment_hash`.
    pub async fn create_invoice_via_lnrpc_v2(
        &self,
        payment_hash: sha256::Hash,
        amount: Amount,
        description: Bolt11InvoiceDescription,
        expiry_time: u32,
    ) -> std::result::Result<Bolt11Invoice, LightningRpcError> {
        let lnrpc = self.get_lightning_context().await?.lnrpc;

        let response = match description {
            Bolt11InvoiceDescription::Direct(description) => {
                lnrpc
                    .create_invoice(CreateInvoiceRequest {
                        payment_hash: Some(payment_hash),
                        amount_msat: amount.msats,
                        expiry_secs: expiry_time,
                        description: Some(InvoiceDescription::Direct(description)),
                    })
                    .await?
            }
            Bolt11InvoiceDescription::Hash(hash) => {
                lnrpc
                    .create_invoice(CreateInvoiceRequest {
                        payment_hash: Some(payment_hash),
                        amount_msat: amount.msats,
                        expiry_secs: expiry_time,
                        description: Some(InvoiceDescription::Hash(hash)),
                    })
                    .await?
            }
        };

        Bolt11Invoice::from_str(&response.invoice).map_err(|e| {
            LightningRpcError::FailedToGetInvoice {
                failure_reason: e.to_string(),
            }
        })
    }

    pub async fn verify_bolt11_preimage_v2(
        &self,
        payment_hash: sha256::Hash,
        wait: bool,
    ) -> std::result::Result<VerifyResponse, String> {
        let registered_contract = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_registered_incoming_contract(PaymentImage::Hash(payment_hash))
            .await
            .ok_or("Unknown payment hash".to_string())?;

        let client = self
            .select_client(registered_contract.federation_id)
            .await
            .map_err(|_| "Not connected to federation".to_string())?
            .into_value();

        let operation_id = OperationId::from_encodable(&registered_contract.contract);

        if !(wait || client.operation_exists(operation_id).await) {
            return Ok(VerifyResponse {
                settled: false,
                preimage: None,
            });
        }

        let state = client
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .await_receive(operation_id)
            .await;

        let preimage = match state {
            FinalReceiveState::Success(preimage) => Ok(preimage),
            FinalReceiveState::Failure => Err("Payment has failed".to_string()),
            FinalReceiveState::Refunded => Err("Payment has been refunded".to_string()),
            FinalReceiveState::Rejected => Err("Payment has been rejected".to_string()),
        }?;

        Ok(VerifyResponse {
            settled: true,
            preimage: Some(preimage),
        })
    }

    /// Retrieves the persisted `CreateInvoicePayload` from the database
    /// specified by the `payment_hash` and the `ClientHandleArc` specified
    /// by the payload's `federation_id`.
    pub async fn get_registered_incoming_contract_and_client_v2(
        &self,
        payment_image: PaymentImage,
        amount_msats: u64,
    ) -> Result<(IncomingContract, ClientHandleArc)> {
        let registered_incoming_contract = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_registered_incoming_contract(payment_image)
            .await
            .ok_or(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "No corresponding decryption contract available".to_string(),
            )))?;

        if registered_incoming_contract.incoming_amount_msats != amount_msats {
            return Err(PublicGatewayError::LNv2(LNv2Error::IncomingPayment(
                "The available decryption contract's amount is not equal to the requested amount"
                    .to_string(),
            )));
        }

        let client = self
            .select_client(registered_incoming_contract.federation_id)
            .await?
            .into_value();

        Ok((registered_incoming_contract.contract, client))
    }
}

#[async_trait]
impl IGatewayClientV2 for Gateway {
    async fn complete_htlc(&self, htlc_response: InterceptPaymentResponse) {
        loop {
            match self.get_lightning_context().await {
                Ok(lightning_context) => {
                    match lightning_context
                        .lnrpc
                        .complete_htlc(htlc_response.clone())
                        .await
                    {
                        Ok(..) => return,
                        Err(err) => {
                            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failure trying to complete payment");
                        }
                    }
                }
                Err(err) => {
                    warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failure trying to complete payment");
                }
            }

            sleep(Duration::from_secs(5)).await;
        }
    }

    async fn is_direct_swap(
        &self,
        invoice: &Bolt11Invoice,
    ) -> anyhow::Result<Option<(IncomingContract, ClientHandleArc)>> {
        let lightning_context = self.get_lightning_context().await?;
        if lightning_context.lightning_public_key == invoice.get_payee_pub_key() {
            let (contract, client) = self
                .get_registered_incoming_contract_and_client_v2(
                    PaymentImage::Hash(*invoice.payment_hash()),
                    invoice
                        .amount_milli_satoshis()
                        .expect("The amount invoice has been previously checked"),
                )
                .await?;
            Ok(Some((contract, client)))
        } else {
            Ok(None)
        }
    }

    async fn pay(
        &self,
        invoice: Bolt11Invoice,
        max_delay: u64,
        max_fee: Amount,
    ) -> std::result::Result<[u8; 32], LightningRpcError> {
        let lightning_context = self.get_lightning_context().await?;
        lightning_context
            .lnrpc
            .pay(invoice, max_delay, max_fee)
            .await
            .map(|response| response.preimage.0)
    }

    async fn min_contract_amount(
        &self,
        federation_id: &FederationId,
        amount: u64,
    ) -> anyhow::Result<Amount> {
        Ok(self
            .routing_info_v2(federation_id)
            .await?
            .ok_or(anyhow!("Routing Info not available"))?
            .send_fee_minimum
            .add_to(amount))
    }

    /// `gatewaydv2` does not support LNv1, so no invoice is ever treated as an
    /// LNv1 invoice and the LNv2 -> LNv1 swap path below is never taken.
    async fn is_lnv1_invoice(&self, _invoice: &Bolt11Invoice) -> Option<Spanned<ClientHandleArc>> {
        None
    }

    /// Unreachable: [`Self::is_lnv1_invoice`] always returns `None`, so the
    /// caller never reaches the LNv1 swap. `gatewaydv2` has no LNv1 module to
    /// perform the swap with.
    async fn relay_lnv1_swap(
        &self,
        _client: &ClientHandleArc,
        _invoice: &Bolt11Invoice,
    ) -> anyhow::Result<FinalReceiveState> {
        bail!("LNv1 swaps are not supported by gatewaydv2")
    }
}
