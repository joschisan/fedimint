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

pub mod cli_server;
pub mod client;
pub mod config;
mod db;
pub mod envs;
mod error;
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
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use async_trait::async_trait;
use bitcoin::hashes::sha256;
use bitcoin::{Address, Network, Txid};
use clap::Parser;
use client::GatewayClientBuilder;
pub use config::GatewayParameters;
use config::{DatabaseBackend, GatewayOpts};
use envs::FM_GATEWAY_SKIP_WAIT_FOR_SYNC_ENV;
use error::FederationNotConnected;
use federation_manager::{FederationManager, transient_federation_config};
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::ClientHandleArc;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::{ModuleKind, OperationId};
use fedimint_core::db::Database;
use fedimint_core::envs::is_env_var_set;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::task::{TaskGroup, TaskHandle, TaskShutdownToken, sleep};
use fedimint_core::time::duration_since_epoch;
use fedimint_core::util::backoff_util::fibonacci_max_one_hour;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow, SafeUrl, Spanned, retry};
use fedimint_core::{
    Amount, BitcoinAmountOrAll, crit, fedimint_build_code_version_env, get_network_for_address,
};
use fedimint_gateway_common::{
    ChainSource, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse, ConnectFedPayload,
    CreateInvoiceForOperatorPayload, DepositAddressPayload, FederationInfo, GatewayFedConfig,
    LeaveFedPayload, LightningInfo, LightningMode, MnemonicResponse, OpenChannelRequest,
    PayInvoiceForOperatorPayload, ReceiveEcashPayload, ReceiveEcashResponse, SendOnchainRequest,
    SpendEcashPayload, SpendEcashResponse, V1_API_ENDPOINT, WithdrawPayload, WithdrawResponse,
};
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

#[bon::bon]
impl Gateway {
    /// Construct a [`Gateway`] using a fluent builder API.
    ///
    /// # Example
    /// ```ignore
    /// let gateway = Gateway::builder(lightning_mode, client_builder, gateway_db)
    ///     .listen(addr)
    ///     .api_addr(url)
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

    /// The Bitcoin network that the Lightning network is configured to.
    network: Network,

    /// The source of the Bitcoin blockchain data
    chain_source: ChainSource,

    /// The default routing fees for new federations
    default_routing_fees: PaymentFee,

    /// The default transaction fees for new federations
    default_transaction_fees: PaymentFee,

    /// The gateway's advertised HTTP API url (from `FM_GATEWAY_API_ADDR`),
    /// surfaced by the `info` admin command.
    api_url: Option<SafeUrl>,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("federation_manager", &self.federation_manager)
            .field("state", &self.state)
            .field("client_builder", &self.client_builder)
            .field("gateway_db", &self.gateway_db)
            .field("listen", &self.listen)
            .field("api_url", &self.api_url)
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
    /// Builds the bitcoind chain source for the LDK node from the configured
    /// RPC credentials. LDK uses bitcoind purely as a chain-data source via
    /// node-level RPCs, so -- unlike v1 gatewayd -- no watch-only wallet is
    /// created.
    fn bitcoind_chain_source(opts: &GatewayOpts) -> ChainSource {
        ChainSource::Bitcoind {
            username: opts
                .bitcoind_username
                .clone()
                .expect("FM_BITCOIND_URL is set but FM_BITCOIND_USERNAME is not"),
            password: opts
                .bitcoind_password
                .clone()
                .expect("FM_BITCOIND_URL is set but FM_BITCOIND_PASSWORD is not"),
            server_url: opts.bitcoind_url.clone().expect("No bitcoind url set"),
        }
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

        let chain_source = match (opts.bitcoind_url.as_ref(), opts.esplora_url.as_ref()) {
            // Use bitcoind by default if both are set
            (Some(_), _) => Self::bitcoind_chain_source(&opts),
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

        Self {
            federation_manager: Arc::new(RwLock::new(FederationManager::new())),
            lightning_mode,
            state: Arc::new(RwLock::new(gateway_state)),
            client_builder,
            gateway_db,
            listen: gateway_parameters.listen,
            metrics_listen: gateway_parameters.metrics_listen,
            task_group,
            network,
            chain_source,
            default_routing_fees: gateway_parameters.default_routing_fees,
            default_transaction_fees: gateway_parameters.default_transaction_fees,
            api_url: gateway_parameters.versioned_api,
        }
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
        // Federation clients are loaded lazily on first use; nothing is built
        // at boot.
        self.start_gateway(runtime);
        // start metrics server
        fedimint_metrics::spawn_api_server(self.metrics_listen, self.task_group.clone()).await?;
        // start webserver last to avoid handling requests before fully initialized
        let handle = self.task_group.make_handle();
        let gateway = Arc::new(self);
        crate::cli_server::run_cli_server(gateway.clone())?;
        run_webserver(gateway).await?;
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
            self.select_client(federation_id).await?;
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

    /// Retrieves the client for a federation, building it on demand if it is
    /// not yet loaded. Clients are loaded lazily — nothing is built at boot;
    /// each federation's client is constructed the first time it is needed and
    /// cached thereafter (double-checked locking).
    pub async fn select_client(
        &self,
        federation_id: FederationId,
    ) -> std::result::Result<Spanned<fedimint_client::ClientHandleArc>, FederationNotConnected>
    {
        let not_connected = || FederationNotConnected {
            federation_id_prefix: federation_id.to_prefix(),
        };

        // Fast path: the client is already loaded.
        if let Some(client) = self
            .federation_manager
            .read()
            .await
            .client(&federation_id)
            .cloned()
        {
            return Ok(client);
        }

        // Slow path: build the client under the write lock.
        let mut federation_manager = self.federation_manager.write().await;

        // Re-check in case another task built it while we waited for the lock.
        if let Some(client) = federation_manager.client(&federation_id).cloned() {
            return Ok(client);
        }

        // Only federations whose config is persisted can be built. The stored
        // `ClientConfig` is what lets the client be (lazily) joined without an
        // invite code.
        let Some(config) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_client_config(federation_id)
            .await
        else {
            return Err(not_connected());
        };

        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");

        let client = Box::pin(Spanned::try_new(
            info_span!(target: LOG_GATEWAY, "client", federation_id = %federation_id),
            self.client_builder
                .build(federation_id, config, Arc::new(self.clone()), &mnemonic),
        ))
        .await
        .map_err(|err| {
            warn!(
                target: LOG_GATEWAY,
                federation_id = %federation_id,
                err = %err,
                "Failed to lazily load federation client"
            );
            not_connected()
        })?;

        federation_manager.add_client(client.clone());
        Ok(client)
    }

    async fn load_mnemonic(gateway_db: &Database) -> Option<Mnemonic> {
        let entropy = gateway_db
            .begin_transaction_nc()
            .await
            .load_root_entropy()
            .await?;
        Mnemonic::from_entropy(&entropy).ok()
    }

    /// Reconstructs a single invite code from a stored `ClientConfig` (first
    /// peer). The gateway does not persist invite codes; this is only used to
    /// populate the transient config in `FederationInfo` responses.
    fn invite_from_config(config: &ClientConfig) -> Option<InviteCode> {
        let federation_id = config.calculate_federation_id();
        config
            .global
            .api_endpoints
            .iter()
            .next()
            .map(|(peer, url)| InviteCode::new(url.url.clone(), *peer, federation_id, None))
    }

    /// Builds a [`FederationInfo`] purely from a stored `ClientConfig`, without
    /// loading the client. The listing is balance-free (picomint-style):
    /// `balance_msat` is always zero — read an actual balance with the
    /// per-federation balance command, which lazily loads the client.
    fn federation_info_from_config(
        &self,
        federation_id: FederationId,
        config: &ClientConfig,
    ) -> Option<FederationInfo> {
        let invite_code = Self::invite_from_config(config)?;
        Some(FederationInfo {
            federation_id,
            federation_name: config.global.federation_name().map(str::to_string),
            balance_msat: Amount::ZERO,
            config: transient_federation_config(
                invite_code,
                self.default_routing_fees,
                self.default_transaction_fees,
            ),
            last_backup_time: None,
        })
    }

    /// Lists every joined federation by reading its `ClientConfig` from the
    /// database. Never builds a client; balances are best-effort (see
    /// [`Gateway::federation_info_from_config`]).
    async fn list_federation_infos(&self) -> Vec<FederationInfo> {
        let configs = {
            let mut dbtx = self.gateway_db.begin_transaction_nc().await;
            dbtx.load_client_configs().await
        };
        let mut federation_infos = Vec::new();
        for (federation_id, config) in configs {
            if let Some(info) = self.federation_info_from_config(federation_id, &config) {
                federation_infos.push(info);
            }
        }
        federation_infos
    }

    /// Ensures the federation exposes the three v2 modules the gateway needs.
    ///
    /// This checks module *kinds* only, which works on the raw, undecoded
    /// config returned by `preview`, so it can run at connect time without
    /// building a client. The network is intentionally not validated here; a
    /// mismatch surfaces later when the client is built and operates.
    fn ensure_v2_modules(config: &ClientConfig) -> AdminResult<()> {
        for kind in ["lnv2", "mintv2", "walletv2"] {
            let module_kind = ModuleKind::from_static_str(kind);
            if !config.modules.values().any(|m| m.kind == module_kind) {
                return Err(AdminGatewayError::ClientCreationError(anyhow!(
                    "Federation {} is missing the required {kind} module",
                    config.calculate_federation_id()
                )));
            }
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
    /// Returns a list of Lightning network channels from the Gateway's
    /// Lightning node.
    async fn handle_list_channels_msg(
        &self,
    ) -> AdminResult<Vec<fedimint_gateway_common::ChannelInfo>> {
        let context = self.get_lightning_context().await?;
        let response = context.lnrpc.list_channels().await?;
        Ok(response.channels)
    }

    /// Handle a request to have the Gateway leave a federation. The Gateway
    /// will request the federation to remove the registration record and
    /// the gateway will remove the configuration needed to construct the
    /// federation client.
    /// "Leaving" a federation only disables it: its public-facing endpoints are
    /// gated off, but the client and its config are retained so in-flight
    /// payments settle and it can be re-enabled by connecting again. Because
    /// federation state is never discarded, the gateway never needs the
    /// recovery protocol.
    async fn handle_leave_federation(
        &self,
        payload: LeaveFedPayload,
    ) -> AdminResult<FederationInfo> {
        let federation_id = payload.federation_id;
        let not_connected = || FederationNotConnected {
            federation_id_prefix: federation_id.to_prefix(),
        };

        let mut dbtx = self.gateway_db.begin_transaction().await;
        let config = dbtx
            .load_client_config(federation_id)
            .await
            .ok_or_else(not_connected)?;
        dbtx.save_disabled_federation(federation_id).await;
        dbtx.commit_tx().await;

        self.federation_info_from_config(federation_id, &config)
            .ok_or_else(not_connected)
            .map_err(AdminGatewayError::from)
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
        let not_connected = || FederationNotConnected {
            federation_id_prefix: federation_id.to_prefix(),
        };

        // Connecting (re-)enables a federation: clear any disabled flag. A
        // federation is only ever disabled, never truly left, so reconnecting
        // an existing one just re-enables it rather than rejoining — which is
        // why the gateway never needs the recovery protocol.
        {
            let mut dbtx = self.gateway_db.begin_transaction().await;
            dbtx.remove_disabled_federation(federation_id).await;
            dbtx.commit_tx().await;
        }

        // If the config is already persisted, connecting simply re-enabled the
        // federation; no need to re-download or build (the client loads lazily
        // on first use).
        if let Some(config) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_client_config(federation_id)
            .await
        {
            return self
                .federation_info_from_config(federation_id, &config)
                .ok_or_else(not_connected)
                .map_err(AdminGatewayError::from);
        }

        // Fresh connection: download and persist the config WITHOUT building a
        // client. The client (and its first join) happens lazily on first use.
        let config = self
            .client_builder
            .download_config(&invite_code, Arc::new(self.clone()))
            .await?;

        Self::ensure_v2_modules(&config)?;

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.save_client_config(&federation_id, &config).await;
        dbtx.commit_tx().await;
        debug!(
            target: LOG_GATEWAY,
            federation_id = %federation_id,
            "Federation connected"
        );

        self.federation_info_from_config(federation_id, &config)
            .ok_or_else(not_connected)
            .map_err(AdminGatewayError::from)
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
        let all_federations = {
            let mut dbtx = self.gateway_db.begin_transaction_nc().await;
            dbtx.load_client_configs()
                .await
                .into_keys()
                .collect::<BTreeSet<_>>()
        };
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
}

// LNv2 Gateway implementation
impl Gateway {
    /// Returns payment information that LNv2 clients can use to instruct this
    /// Gateway to pay an invoice or receive a payment. Returns `None` if the
    /// federation is disabled or not joined; the client is lazily built to read
    /// its module public key.
    pub async fn routing_info_v2(
        &self,
        federation_id: &FederationId,
    ) -> Result<Option<RoutingInfo>> {
        let context = self.get_lightning_context().await?;

        // Disabled federations advertise no routing info.
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .is_federation_disabled(*federation_id)
            .await
        {
            return Ok(None);
        }

        // Lazily load the client to read its module public key. A federation
        // that isn't joined (or fails to load) yields `None`.
        let Ok(client) = self.select_client(*federation_id).await else {
            return Ok(None);
        };
        let module_public_key = client
            .value()
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .keypair
            .public_key();

        let lightning_fee = self.default_routing_fees;
        let transaction_fee = self.default_transaction_fees;

        Ok(Some(RoutingInfo {
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
