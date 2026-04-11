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
pub mod envs;
mod error;
mod events;
mod federation_manager;
pub mod ldk;
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

use anyhow::{Context, anyhow};
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
use fedimint_bip39::{Bip39RootSecretStrategy, Language, Mnemonic};
use fedimint_bitcoind::bitcoincore::BitcoindClient;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc};
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::db::{Database, DatabaseTransaction, apply_migrations};
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
use fedimint_eventlog::{DBTransactionEventLogExt, EventLogId};
use fedimint_gateway_common::{
    ChainSource, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse, ConnectFedPayload,
    ConnectorType, CreateInvoiceForOperatorPayload, CreateInvoiceRequest, DepositAddressPayload,
    DepositAddressRecheckPayload, FederationBalanceInfo, FederationConfig, FederationInfo,
    GatewayBalances, GatewayFedConfig, GatewayInfo, GetInvoiceRequest, GetInvoiceResponse,
    InterceptPaymentRequest, InterceptPaymentResponse, InvoiceDescription, LightningContext,
    LightningInfo, LightningMode, LightningRpcError, ListTransactionsPayload,
    ListTransactionsResponse, MnemonicResponse, OpenChannelRequest, PayInvoiceForOperatorPayload,
    PaymentAction, PaymentFee, PaymentLogPayload, PaymentLogResponse, PeginFromOnchainPayload,
    Preimage, ReceiveEcashPayload, ReceiveEcashResponse, RegisteredProtocol, RouteHtlcStream,
    SendOnchainRequest, SetFeesPayload, SpendEcashPayload, SpendEcashResponse, V1_API_ENDPOINT,
    WithdrawPayload, WithdrawPreviewPayload, WithdrawPreviewResponse, WithdrawResponse,
    WithdrawToOnchainPayload,
};
use fedimint_gateway_server_db::{GatewayDbtxNcExt as _, get_gatewayd_database_migrations};

#[async_trait]
pub trait IAdminGateway {
    type Error;

    async fn handle_get_info(&self) -> std::result::Result<GatewayInfo, Self::Error>;

    async fn handle_list_channels_msg(
        &self,
    ) -> std::result::Result<Vec<fedimint_gateway_common::ChannelInfo>, Self::Error>;

    async fn handle_connect_federation(
        &self,
        payload: ConnectFedPayload,
    ) -> std::result::Result<FederationInfo, Self::Error>;

    async fn handle_set_fees_msg(
        &self,
        payload: SetFeesPayload,
    ) -> std::result::Result<(), Self::Error>;

    async fn handle_mnemonic_msg(&self) -> std::result::Result<MnemonicResponse, Self::Error>;

    async fn handle_open_channel_msg(
        &self,
        payload: OpenChannelRequest,
    ) -> std::result::Result<Txid, Self::Error>;

    async fn handle_close_channels_with_peer_msg(
        &self,
        payload: CloseChannelsWithPeerRequest,
    ) -> std::result::Result<CloseChannelsWithPeerResponse, Self::Error>;

    async fn handle_get_balances_msg(&self) -> std::result::Result<GatewayBalances, Self::Error>;

    async fn handle_send_onchain_msg(
        &self,
        payload: SendOnchainRequest,
    ) -> std::result::Result<Txid, Self::Error>;

    async fn handle_get_ln_onchain_address_msg(&self) -> std::result::Result<Address, Self::Error>;

    async fn handle_deposit_address_msg(
        &self,
        payload: DepositAddressPayload,
    ) -> std::result::Result<Address, Self::Error>;

    async fn handle_receive_ecash_msg(
        &self,
        payload: ReceiveEcashPayload,
    ) -> std::result::Result<ReceiveEcashResponse, Self::Error>;

    async fn handle_create_invoice_for_operator_msg(
        &self,
        payload: CreateInvoiceForOperatorPayload,
    ) -> std::result::Result<Bolt11Invoice, Self::Error>;

    async fn handle_pay_invoice_for_operator_msg(
        &self,
        payload: PayInvoiceForOperatorPayload,
    ) -> std::result::Result<Preimage, Self::Error>;

    async fn handle_list_transactions_msg(
        &self,
        payload: ListTransactionsPayload,
    ) -> std::result::Result<ListTransactionsResponse, Self::Error>;

    async fn handle_spend_ecash_msg(
        &self,
        payload: SpendEcashPayload,
    ) -> std::result::Result<SpendEcashResponse, Self::Error>;

    async fn handle_shutdown_msg(
        &self,
        task_group: TaskGroup,
    ) -> std::result::Result<(), Self::Error>;

    fn get_task_group(&self) -> TaskGroup;

    async fn handle_withdraw_msg(
        &self,
        payload: WithdrawPayload,
    ) -> std::result::Result<WithdrawResponse, Self::Error>;

    async fn handle_withdraw_preview_msg(
        &self,
        payload: WithdrawPreviewPayload,
    ) -> std::result::Result<WithdrawPreviewResponse, Self::Error>;

    async fn handle_payment_log_msg(
        &self,
        payload: PaymentLogPayload,
    ) -> std::result::Result<PaymentLogResponse, Self::Error>;

    async fn handle_export_invite_codes(
        &self,
    ) -> BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>>;

    fn get_password_hash(&self) -> String;

    fn gatewayd_version(&self) -> String;

    async fn get_chain_source(&self) -> (ChainSource, Network);
}
use fedimint_gwv2_client::{
    EXPIRATION_DELTA_MINIMUM_V2, FinalReceiveState, GatewayClientModuleV2, IGatewayClientV2,
};
use fedimint_lnurl::VerifyResponse;
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::{
    CreateBolt11InvoicePayload, RoutingInfo, SendPaymentPayload,
};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::{MintClientInit, MintClientModule};
use futures::stream::StreamExt;
use lightning_invoice::Bolt11Invoice;
use rand::rngs::OsRng;
use tokio::sync::RwLock;
use tracing::{debug, info, info_span, warn};

use crate::envs::FM_GATEWAY_MNEMONIC_ENV;
use crate::error::{AdminGatewayError, LNv2Error, PublicGatewayError};
use crate::ldk::GatewayLdkClient;
use crate::rpc_server::run_webserver;
use crate::types::PrettyInterceptPaymentRequest;

/// The default number of route hints that the legacy gateway provides for
/// invoice creation.
const DEFAULT_NUM_ROUTE_HINTS: u32 = 1;

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

/// Helper struct for storing the registration parameters for LNv1 for each
/// network protocol.
#[derive(Debug, Clone)]
struct Registration {
    /// The url to advertise in the registration that clients can use to connect
    endpoint_url: SafeUrl,

    /// Keypair that was used to register the gateway registration
    keypair: secp256k1::Keypair,
}

impl Registration {
    pub async fn new(db: &Database, endpoint_url: SafeUrl, protocol: RegisteredProtocol) -> Self {
        let keypair = Gateway::load_or_create_gateway_keypair(db, protocol).await;
        Self {
            endpoint_url,
            keypair,
        }
    }
}

#[bon::bon]
impl Gateway {
    /// Construct a [`Gateway`] using a fluent builder API.
    ///
    /// # Example
    /// ```ignore
    /// let gateway = Gateway::builder(client_builder, gateway_db)
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
    pub async fn new_with_builder(
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

        Gateway::new(
            GatewayParameters {
                listen,
                versioned_api,
                bcrypt_password_hash,
                bcrypt_liquidity_manager_password_hash,
                network,
                num_route_hints: DEFAULT_NUM_ROUTE_HINTS,
                default_routing_fees,
                default_transaction_fees,
                metrics_listen,
            },
            gateway_db,
            client_builder,
            gateway_state,
            chain_source,
        )
        .await
    }
}

/// The action to take after handling a payment stream.
enum ReceivePaymentStreamAction {
    RetryAfterDelay,
    NoRetry,
}

/// LDK-specific configuration parameters.
struct LdkConfig {
    lightning_port: u16,
    alias: String,
}

#[derive(Clone)]
pub struct Gateway {
    /// The gateway's federation manager.
    federation_manager: Arc<RwLock<FederationManager>>,

    /// The LDK lightning client, available once connected.
    ldk_client: Arc<RwLock<Option<Arc<GatewayLdkClient>>>>,

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

/// Internal helper for on-chain withdrawal calculations
struct WithdrawDetails {
    amount: Amount,
    mint_fees: Option<Amount>,
    peg_out_fees: bitcoin::Amount,
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
        .send(address.as_unchecked().clone(), withdraw_amount, Some(fee))
        .await
        .map_err(|e| AdminGatewayError::WithdrawError {
            failure_reason: e.to_string(),
        })?;

    let result = wallet_module
        .await_final_send_operation_state(operation_id)
        .await;

    let fees = fee;

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

/// Calculates an estimated max withdrawable amount on-chain
async fn calculate_max_withdrawable(
    client: &ClientHandleArc,
    _address: &Address,
) -> AdminResult<WithdrawDetails> {
    let balance = client.get_balance_for_btc().await.map_err(|err| {
        AdminGatewayError::Unexpected(anyhow!(
            "Balance not available: {}",
            err.fmt_compact_anyhow()
        ))
    })?;

    let wallet_module = client
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| AdminGatewayError::Unexpected(anyhow!("No wallet module found")))?;

    let peg_out_fees =
        wallet_module
            .send_fee()
            .await
            .map_err(|e| AdminGatewayError::WithdrawError {
                failure_reason: e.to_string(),
            })?;

    let max_withdrawable_before_mint_fees =
        balance.checked_sub(peg_out_fees.into()).ok_or_else(|| {
            AdminGatewayError::WithdrawError {
                failure_reason: "Insufficient balance to cover peg-out fees".to_string(),
            }
        })?;

    let mint_fees = Amount::ZERO;

    let max_withdrawable = max_withdrawable_before_mint_fees.saturating_sub(mint_fees);

    Ok(WithdrawDetails {
        amount: max_withdrawable,
        mint_fees: Some(mint_fees),
        peg_out_fees,
    })
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

        // Apply database migrations before using the database to ensure old database
        // structures are readable.
        apply_migrations(
            &gateway_db,
            (),
            "gatewayd".to_string(),
            get_gatewayd_database_migrations(),
            None,
            None,
        )
        .await?;

        // For legacy reasons, we use the http id for the unique identifier of the
        // bitcoind watch-only wallet
        let http_id = Self::load_or_create_gateway_keypair(&gateway_db, RegisteredProtocol::Http)
            .await
            .public_key();
        let chain_source = match (opts.bitcoind_url.as_ref(), opts.esplora_url.as_ref()) {
            (Some(_), None) => {
                let (_client, chain_source) =
                    Self::get_bitcoind_client(&opts, gateway_parameters.network, &http_id)?;
                chain_source
            }
            (None, Some(url)) => ChainSource::Esplora {
                server_url: url.clone(),
            },
            (Some(_), Some(_)) => {
                // Use bitcoind by default if both are set
                let (_client, chain_source) =
                    Self::get_bitcoind_client(&opts, gateway_parameters.network, &http_id)?;
                chain_source
            }
            _ => unreachable!("ArgGroup already enforced XOR relation"),
        };

        // Gateway module will be attached when the federation clients are created
        let mut registry = ClientModuleInitRegistry::new();
        registry.attach(MintClientInit);
        registry.attach(MintClientInit);
        registry.attach(fedimint_walletv2_client::WalletClientInit);

        let client_builder =
            GatewayClientBuilder::new(opts.data_dir.clone(), registry, opts.db_backend).await?;

        if Self::load_mnemonic(&gateway_db).await.is_none() {
            let mnemonic = if let Ok(words) = std::env::var(FM_GATEWAY_MNEMONIC_ENV) {
                info!(target: LOG_GATEWAY, "Using provided mnemonic from environment variable");
                Mnemonic::parse_in_normalized(Language::English, words.as_str()).map_err(|e| {
                    AdminGatewayError::MnemonicError(anyhow!(format!(
                        "Seed phrase provided in environment was invalid {e:?}"
                    )))
                })?
            } else {
                debug!(target: LOG_GATEWAY, "Generating mnemonic and writing entropy to client storage");
                Bip39RootSecretStrategy::<12>::random(&mut OsRng)
            };

            Client::store_encodable_client_secret(&gateway_db, mnemonic.to_entropy())
                .await
                .map_err(AdminGatewayError::MnemonicError)?;
        }
        let gateway_state = GatewayState::Disconnected;

        info!(
            target: LOG_GATEWAY,
            version = %fedimint_build_code_version_env!(),
            "Starting gatewayd",
        );

        Gateway::new(
            gateway_parameters,
            gateway_db,
            client_builder,
            gateway_state,
            chain_source,
        )
        .await
    }

    /// Helper function for creating a gateway from either
    /// `new_with_default_modules` or `Gateway::builder`.
    async fn new(
        gateway_parameters: GatewayParameters,
        gateway_db: Database,
        client_builder: GatewayClientBuilder,
        gateway_state: GatewayState,
        chain_source: ChainSource,
    ) -> anyhow::Result<Gateway> {
        let network = gateway_parameters.network;

        let task_group = TaskGroup::new();
        task_group.install_kill_handler();

        let mut registrations = BTreeMap::new();
        if let Some(http_url) = gateway_parameters.versioned_api {
            registrations.insert(
                RegisteredProtocol::Http,
                Registration::new(&gateway_db, http_url, RegisteredProtocol::Http).await,
            );
        }

        Ok(Self {
            federation_manager: Arc::new(RwLock::new(FederationManager::new())),
            ldk_client: Arc::new(RwLock::new(None)),
            state: Arc::new(RwLock::new(gateway_state)),
            client_builder,
            gateway_db: gateway_db.clone(),
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
        })
    }

    async fn load_or_create_gateway_keypair(
        gateway_db: &Database,
        protocol: RegisteredProtocol,
    ) -> secp256k1::Keypair {
        let mut dbtx = gateway_db.begin_transaction().await;
        let keypair = dbtx.load_or_create_gateway_keypair(protocol).await;
        dbtx.commit_tx().await;
        keypair
    }

    pub async fn http_gateway_id(&self) -> PublicKey {
        Self::load_or_create_gateway_keypair(&self.gateway_db, RegisteredProtocol::Http)
            .await
            .public_key()
    }

    async fn get_state(&self) -> GatewayState {
        self.state.read().await.clone()
    }

    /// Reads and serializes structures from the Gateway's database for the
    /// purpose for serializing to JSON for inspection.
    pub async fn dump_database(
        dbtx: &mut DatabaseTransaction<'_>,
        prefix_names: Vec<String>,
    ) -> BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> {
        dbtx.dump_database(prefix_names).await
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
        self.register_clients_timer();
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
        let _tg = self.task_group.clone();
        self.task_group.spawn(
            "Subscribe to intercepted lightning payments in stream",
            |handle| async move {
                // Repeatedly attempt to establish a connection to the lightning node and create a payment stream, re-trying if the connection is broken.
                loop {
                    if handle.is_shutting_down() {
                        info!(target: LOG_GATEWAY, "Gateway lightning payment stream handler loop is shutting down");
                        break;
                    }

                    let mnemonic = Self::load_mnemonic(&self_copy.gateway_db)
                        .await
                        .expect("mnemonic should be set");

                    let ldk_config = Self::get_ldk_config();

                    // Create LDK client with retry
                    let mut ldk_client = match retry(
                        "create LDK Node",
                        fibonacci_max_one_hour(),
                        || async {
                            GatewayLdkClient::new(
                                &self_copy.client_builder.data_dir().join(LDK_NODE_DB_FOLDER),
                                self_copy.chain_source.clone(),
                                self_copy.network,
                                ldk_config.lightning_port,
                                ldk_config.alias.clone(),
                                mnemonic.clone(),
                                runtime.clone(),
                            )
                        },
                    )
                    .await
                    {
                        Ok(client) => client,
                        Err(err) => {
                            warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), "Failed to create LDK client");
                            sleep(Duration::from_secs(PAYMENT_STREAM_RETRY_SECONDS)).await;
                            continue;
                        }
                    };

                    debug!(target: LOG_GATEWAY, "Establishing lightning payment stream...");
                    let stream = match ldk_client.route_htlcs() {
                        Ok(stream) => stream,
                        Err(err) => {
                            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to open lightning payment stream");
                            sleep(Duration::from_secs(PAYMENT_STREAM_RETRY_SECONDS)).await;
                            continue
                        }
                    };

                    let ldk_client = Arc::new(ldk_client);

                    // Successful calls to `route_htlcs` establish a connection
                    self_copy.set_gateway_state(GatewayState::Connected).await;
                    *self_copy.ldk_client.write().await = Some(ldk_client.clone());
                    info!(target: LOG_GATEWAY, "Established lightning payment stream");

                    let route_payments_response =
                        self_copy.route_lightning_payments(&handle, stream, &ldk_client).await;

                    self_copy.set_gateway_state(GatewayState::Disconnected).await;
                    *self_copy.ldk_client.write().await = None;

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

    /// Returns the LDK configuration from environment variables.
    fn get_ldk_config() -> LdkConfig {
        let lightning_port = env::var("FM_PORT_LDK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9735);
        let alias = env::var("FM_LDK_ALIAS").unwrap_or_default();
        LdkConfig {
            lightning_port,
            alias,
        }
    }

    /// Handles a stream of incoming payments from the lightning node after
    /// ensuring the gateway is properly configured. Awaits until the stream
    /// is closed, then returns with the appropriate action to take.
    async fn route_lightning_payments<'a>(
        &'a self,
        handle: &TaskHandle,
        mut stream: RouteHtlcStream<'a>,
        ldk_client: &Arc<GatewayLdkClient>,
    ) -> ReceivePaymentStreamAction {
        let LightningInfo::Connected {
            public_key: lightning_public_key,
            alias: lightning_alias,
            network: lightning_network,
            block_height: _,
            synced_to_chain,
        } = ldk_client.parsed_node_info().await
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
            if let Err(err) = ldk_client.wait_for_chain_sync().await {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to wait for chain sync");
                return ReceivePaymentStreamAction::RetryAfterDelay;
            }
        }

        let lightning_context = LightningContext {
            lightning_public_key,
            lightning_alias,
            lightning_network,
            supports_private_payments: ldk_client.supports_private_payments(),
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
                    if let GatewayState::Running { .. } = *state_guard {
                        // Spawn a subtask to handle each payment in parallel
                        let gateway = self.clone();
                        htlc_task_group.spawn_cancellable_silent(
                            "handle_lightning_payment",
                            async move {
                                let start = fedimint_core::time::now();
                                let outcome = gateway
                                    .handle_lightning_payment(payment_request)
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
    /// payment off to it. Otherwise, forwards the payment to the next hop like
    /// a normal lightning node.
    ///
    /// Returns the outcome label for metrics tracking.
    async fn handle_lightning_payment(
        &self,
        payment_request: InterceptPaymentRequest,
    ) -> &'static str {
        info!(
            target: LOG_GATEWAY,
            lightning_payment = %PrettyInterceptPaymentRequest(&payment_request),
            "Intercepting lightning payment",
        );

        let lnv2_start = fedimint_core::time::now();
        let lnv2_result = self
            .try_handle_lightning_payment_lnv2(&payment_request)
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

        Self::forward_lightning_payment(payment_request, &*self.get_ldk_client().await).await;
        "forward"
    }

    /// Tries to handle a lightning payment using the LNv2 protocol.
    /// Returns `Ok` if the payment was handled, `Err` otherwise.
    async fn try_handle_lightning_payment_lnv2(
        &self,
        htlc_request: &InterceptPaymentRequest,
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

            if let Err(err) = self.get_ldk_client().await.complete_htlc(outcome).await {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Error sending HTLC response to lightning node");
            }
        }

        Ok(())
    }

    /// Forwards a lightning payment to the next hop like a normal lightning
    /// node.
    async fn forward_lightning_payment(
        htlc_request: InterceptPaymentRequest,
        ldk_client: &GatewayLdkClient,
    ) {
        let outcome = InterceptPaymentResponse {
            action: PaymentAction::Forward,
            payment_hash: htlc_request.payment_hash,
            incoming_chan_id: htlc_request.incoming_chan_id,
            htlc_id: htlc_request.htlc_id,
        };

        if let Err(err) = ldk_client.complete_htlc(outcome).await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Error sending lightning payment response to lightning node");
        }
    }

    /// Returns the active LDK client, panicking if not connected.
    async fn get_ldk_client(&self) -> Arc<GatewayLdkClient> {
        self.ldk_client
            .read()
            .await
            .clone()
            .expect("LDK client should be available when gateway is running")
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

        let wallet_module = client
            .value()
            .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
            .map_err(|_| AdminGatewayError::Unexpected(anyhow!("No wallet module found")))?;

        Ok(wallet_module.receive().await)
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
        let ecash: fedimint_mintv2_client::ECash =
            base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes).map_err(|e| {
                PublicGatewayError::ReceiveEcashError {
                    failure_reason: format!("Invalid ECash: {e}"),
                }
            })?;

        let federation_id_prefix = ecash.mint().map(|id| id.to_prefix()).ok_or_else(|| {
            PublicGatewayError::ReceiveEcashError {
                failure_reason: "ECash does not contain federation id".to_string(),
            }
        })?;

        let client = self
            .federation_manager
            .read()
            .await
            .get_client_for_federation_id_prefix(federation_id_prefix)
            .ok_or(FederationNotConnected {
                federation_id_prefix,
            })?;

        let mint = client.value().get_first_module::<MintClientModule>()?;
        let amount = ecash.amount();

        let operation_id = mint
            .receive(ecash, serde_json::Value::Null)
            .await
            .map_err(|e| PublicGatewayError::ReceiveEcashError {
                failure_reason: e.to_string(),
            })?;

        if payload.wait {
            match mint.await_final_receive_operation_state(operation_id).await {
                fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
                fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
                    return Err(PublicGatewayError::ReceiveEcashError {
                        failure_reason: "ECash receive was rejected".to_string(),
                    });
                }
            }
        }

        Ok(ReceiveEcashResponse { amount })
    }

    /// Retrieves an invoice by the payment hash if it exists, otherwise returns
    /// `None`.
    pub async fn handle_get_invoice_msg(
        &self,
        payload: GetInvoiceRequest,
    ) -> AdminResult<Option<GetInvoiceResponse>> {
        let ldk_client = self.get_ldk_client().await;
        let invoice = ldk_client.get_invoice(payload).await?;
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

    /// Registers the gateway with each specified federation.
    /// This is a no-op for LDK since it does not use LNv1 registration.
    async fn register_federations(
        &self,
        _federations: &BTreeMap<FederationId, FederationConfig>,
        _register_task_group: &TaskGroup,
    ) {
        // LDK does not use LNv1 gateway registration
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
        let secret = Client::load_decodable_client_secret::<Vec<u8>>(gateway_db)
            .await
            .ok()?;
        Mnemonic::from_entropy(&secret).ok()
    }

    /// Reads the connected federation client configs from the Gateway's
    /// database and reconstructs the clients necessary for interacting with
    /// connection federations.
    async fn load_clients(&self) -> AdminResult<()> {
        let mut federation_manager = self.federation_manager.write().await;

        let configs = {
            let mut dbtx = self.gateway_db.begin_transaction_nc().await;
            dbtx.load_federation_configs().await
        };

        if let Some(max_federation_index) = configs.values().map(|cfg| cfg.federation_index).max() {
            federation_manager.set_next_index(max_federation_index + 1);
        }

        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");

        for (federation_id, config) in configs {
            let federation_index = config.federation_index;
            match Box::pin(Spanned::try_new(
                info_span!(target: LOG_GATEWAY, "client", federation_id  = %federation_id.clone()),
                self.client_builder
                    .build(config, Arc::new(self.clone()), &mnemonic),
            ))
            .await
            {
                Ok(client) => {
                    federation_manager.add_client(federation_index, client);
                }
                _ => {
                    warn!(target: LOG_GATEWAY, federation_id = %federation_id, "Failed to load client");
                }
            }
        }

        Ok(())
    }

    /// Legacy mechanism for registering the Gateway with connected federations.
    /// This is a no-op for LDK since it does not use LNv1 registration.
    fn register_clients_timer(&self) {
        // LDK does not use LNv1 gateway registration
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
        if lnv2_cfg.is_none() {
            return Err(AdminGatewayError::ClientCreationError(anyhow!(
                "Federation {federation_id} does not have an LNv2 lightning module"
            )));
        }

        // Verify the LNv2 network
        if let Some(cfg) = lnv2_cfg {
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

    /// Iterates through all of the federations the gateway is registered with
    /// and requests to remove the registration record.
    /// This is a no-op for LDK since it does not use LNv1 registration.
    pub async fn unannounce_from_all_federations(&self) {
        // LDK does not use LNv1 gateway registration
    }
}

#[async_trait]
impl IAdminGateway for Gateway {
    type Error = AdminGatewayError;

    /// Returns information about the Gateway back to the client when requested
    /// via the webserver.
    async fn handle_get_info(&self) -> AdminResult<GatewayInfo> {
        let GatewayState::Running { .. } = self.get_state().await else {
            return Ok(GatewayInfo {
                federations: vec![],
                federation_fake_scids: None,
                version_hash: fedimint_build_code_version_env!().to_string(),
                gateway_state: self.state.read().await.to_string(),
                lightning_info: LightningInfo::NotConnected,
                lightning_mode: LightningMode::Ldk {
                    lightning_port: Self::get_ldk_config().lightning_port,
                    alias: Self::get_ldk_config().alias,
                },
                registrations: self
                    .registrations
                    .iter()
                    .map(|(k, v)| (k.clone(), (v.endpoint_url.clone(), v.keypair.public_key())))
                    .collect(),
            });
        };

        let dbtx = self.gateway_db.begin_transaction_nc().await;
        let federations = self
            .federation_manager
            .read()
            .await
            .federation_info_all_federations(dbtx)
            .await;

        let channels: BTreeMap<u64, FederationId> = federations
            .iter()
            .map(|federation_info| {
                (
                    federation_info.config.federation_index,
                    federation_info.federation_id,
                )
            })
            .collect();

        let ldk_client = self.get_ldk_client().await;
        let lightning_info = ldk_client.parsed_node_info().await;

        Ok(GatewayInfo {
            federations,
            federation_fake_scids: Some(channels),
            version_hash: fedimint_build_code_version_env!().to_string(),
            gateway_state: self.state.read().await.to_string(),
            lightning_info,
            lightning_mode: LightningMode::Ldk {
                lightning_port: Self::get_ldk_config().lightning_port,
                alias: Self::get_ldk_config().alias,
            },
            registrations: self
                .registrations
                .iter()
                .map(|(k, v)| (k.clone(), (v.endpoint_url.clone(), v.keypair.public_key())))
                .collect(),
        })
    }

    /// Returns a list of Lightning network channels from the Gateway's
    /// Lightning node.
    async fn handle_list_channels_msg(
        &self,
    ) -> AdminResult<Vec<fedimint_gateway_common::ChannelInfo>> {
        let ldk_client = self.get_ldk_client().await;
        let response = ldk_client.list_channels().await?;
        Ok(response.channels)
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

        // The gateway deterministically assigns a unique identifier (u64) to each
        // federation connected.
        let federation_index = federation_manager.pop_next_index()?;

        let federation_config = FederationConfig {
            invite_code,
            federation_index,
            lightning_fee: self.default_routing_fees,
            transaction_fee: self.default_transaction_fees,
            // Note: deprecated, unused
            _connector: ConnectorType::Tcp,
        };

        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");
        let recover = payload.recover.unwrap_or(false);
        if recover {
            self.client_builder
                .recover(federation_config.clone(), Arc::new(self.clone()), &mnemonic)
                .await?;
        }

        let client = self
            .client_builder
            .build(federation_config.clone(), Arc::new(self.clone()), &mnemonic)
            .await?;

        if recover {
            client.wait_for_all_active_state_machines().await?;
        }

        // Instead of using `FederationManager::federation_info`, we manually create
        // federation info here because short channel id is not yet persisted.
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
            config: federation_config.clone(),
        };

        Self::check_federation_network(&client, self.network).await?;

        // no need to enter span earlier, because join has a span
        federation_manager.add_client(
            federation_index,
            Spanned::new(
                info_span!(target: LOG_GATEWAY, "client", federation_id=%federation_id.clone()),
                async { client },
            )
            .await,
        );

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.save_federation_config(&federation_config).await;
        dbtx.commit_tx().await;
        debug!(
            target: LOG_GATEWAY,
            federation_id = %federation_id,
            federation_index = %federation_index,
            "Federation connected"
        );

        Ok(federation_info)
    }

    /// Handles a request to change the lightning or transaction fees for all
    /// federations or a federation specified by the `FederationId`.
    async fn handle_set_fees_msg(
        &self,
        SetFeesPayload {
            federation_id,
            lightning_base,
            lightning_parts_per_million,
            transaction_base,
            transaction_parts_per_million,
        }: SetFeesPayload,
    ) -> AdminResult<()> {
        let mut dbtx = self.gateway_db.begin_transaction().await;
        let mut fed_configs = if let Some(fed_id) = federation_id {
            dbtx.load_federation_configs()
                .await
                .into_iter()
                .filter(|(id, _)| *id == fed_id)
                .collect::<BTreeMap<_, _>>()
        } else {
            dbtx.load_federation_configs().await
        };

        let federation_manager = self.federation_manager.read().await;

        for (federation_id, config) in &mut fed_configs {
            let mut lightning_fee = config.lightning_fee;
            if let Some(lightning_base) = lightning_base {
                lightning_fee.base = lightning_base;
            }

            if let Some(lightning_ppm) = lightning_parts_per_million {
                lightning_fee.parts_per_million = lightning_ppm;
            }

            let mut transaction_fee = config.transaction_fee;
            if let Some(transaction_base) = transaction_base {
                transaction_fee.base = transaction_base;
            }

            if let Some(transaction_ppm) = transaction_parts_per_million {
                transaction_fee.parts_per_million = transaction_ppm;
            }

            let client =
                federation_manager
                    .client(federation_id)
                    .ok_or(FederationNotConnected {
                        federation_id_prefix: federation_id.to_prefix(),
                    })?;
            let client_config = client.value().config().await;
            let contains_lnv2 = client_config
                .modules
                .values()
                .any(|m| fedimint_lnv2_common::LightningCommonInit::KIND == m.kind);

            // Check if the lightning fee + transaction fee is higher than the send limit
            let send_fees = lightning_fee + transaction_fee;
            if contains_lnv2 && send_fees.gt(&PaymentFee::SEND_FEE_LIMIT) {
                return Err(AdminGatewayError::GatewayConfigurationError(format!(
                    "Total Send fees exceeded {}",
                    PaymentFee::SEND_FEE_LIMIT
                )));
            }

            // Check if the transaction fee is higher than the receive limit
            if contains_lnv2 && transaction_fee.gt(&PaymentFee::RECEIVE_FEE_LIMIT) {
                return Err(AdminGatewayError::GatewayConfigurationError(format!(
                    "Transaction fees exceeded RECEIVE LIMIT {}",
                    PaymentFee::RECEIVE_FEE_LIMIT
                )));
            }

            config.lightning_fee = lightning_fee;
            config.transaction_fee = transaction_fee;
            dbtx.save_federation_config(config).await;
        }

        dbtx.commit_tx().await;

        Ok(())
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
        let ldk_client = self.get_ldk_client().await;
        let res = ldk_client.open_channel(payload).await?;
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
        let ldk_client = self.get_ldk_client().await;
        let response = ldk_client.close_channels_with_peer(payload.clone()).await?;
        info!(target: LOG_GATEWAY, close_channel_request = %payload, "Initiated channel closure");
        Ok(response)
    }

    /// Returns the ecash, lightning, and onchain balances for the gateway and
    /// the gateway's lightning node.
    async fn handle_get_balances_msg(&self) -> AdminResult<GatewayBalances> {
        let dbtx = self.gateway_db.begin_transaction_nc().await;
        let federation_infos = self
            .federation_manager
            .read()
            .await
            .federation_info_all_federations(dbtx)
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

        let ldk_client = self.get_ldk_client().await;
        let lightning_node_balances = ldk_client.get_balances().await?;

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
        let ldk_client = self.get_ldk_client().await;
        let response = ldk_client.send_onchain(payload.clone()).await?;
        let txid =
            Txid::from_str(&response.txid).map_err(|e| AdminGatewayError::WithdrawError {
                failure_reason: format!("Failed to parse withdrawal TXID: {e}"),
            })?;
        info!(onchain_request = %payload, txid = %txid, "Sent onchain transaction");
        Ok(txid)
    }

    /// Generates an onchain address to fund the gateway's lightning node.
    async fn handle_get_ln_onchain_address_msg(&self) -> AdminResult<Address> {
        let ldk_client = self.get_ldk_client().await;
        let response = ldk_client.get_ln_onchain_address().await?;

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

    async fn handle_deposit_address_msg(
        &self,
        payload: DepositAddressPayload,
    ) -> AdminResult<Address> {
        self.handle_address_msg(payload).await
    }

    async fn handle_receive_ecash_msg(
        &self,
        payload: ReceiveEcashPayload,
    ) -> AdminResult<ReceiveEcashResponse> {
        Self::handle_receive_ecash_msg(self, payload)
            .await
            .map_err(|e| AdminGatewayError::Unexpected(anyhow::anyhow!("{}", e)))
    }

    /// Creates an invoice that is directly payable to the gateway's lightning
    /// node.
    async fn handle_create_invoice_for_operator_msg(
        &self,
        payload: CreateInvoiceForOperatorPayload,
    ) -> AdminResult<Bolt11Invoice> {
        let GatewayState::Running { .. } = self.get_state().await else {
            return Err(AdminGatewayError::Lightning(
                LightningRpcError::FailedToConnect,
            ));
        };

        let ldk_client = self.get_ldk_client().await;
        Bolt11Invoice::from_str(
            &ldk_client
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

        let GatewayState::Running { .. } = self.get_state().await else {
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

        let ldk_client = self.get_ldk_client().await;
        let res = ldk_client
            .pay(payload.invoice, MAX_DELAY, Amount::from_msats(max_fee))
            .await?;
        Ok(res.preimage)
    }

    /// Lists the transactions that the lightning node has made.
    async fn handle_list_transactions_msg(
        &self,
        payload: ListTransactionsPayload,
    ) -> AdminResult<ListTransactionsResponse> {
        let ldk_client = self.get_ldk_client().await;
        let response = ldk_client
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

        let mint_module = client.get_first_module::<MintClientModule>()?;
        let ecash = mint_module
            .send(payload.amount, serde_json::Value::Null)
            .await
            .map_err(|e| AdminGatewayError::Unexpected(e.into()))?;

        Ok(SpendEcashResponse {
            notes: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
        })
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

    fn get_task_group(&self) -> TaskGroup {
        self.task_group.clone()
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
            .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
            .map_err(|_| AdminGatewayError::WithdrawError {
                failure_reason: "No wallet module found".to_string(),
            })?;

        withdraw_v2(client.value(), &wallet_module, &address, amount).await
    }

    /// Returns a preview of the withdrawal fees without executing the
    /// withdrawal. Used by the UI for two-step withdrawal confirmation.
    async fn handle_withdraw_preview_msg(
        &self,
        payload: WithdrawPreviewPayload,
    ) -> AdminResult<WithdrawPreviewResponse> {
        let gateway_network = self.network;
        let address_checked = payload
            .address
            .clone()
            .require_network(gateway_network)
            .map_err(|_| AdminGatewayError::WithdrawError {
                failure_reason: "Address network mismatch".to_string(),
            })?;

        let client = self.select_client(payload.federation_id).await?;

        let WithdrawDetails {
            amount,
            mint_fees,
            peg_out_fees,
        } = match payload.amount {
            BitcoinAmountOrAll::All => {
                calculate_max_withdrawable(client.value(), &address_checked).await?
            }
            BitcoinAmountOrAll::Amount(btc_amount) => {
                let wallet_module = client
                    .value()
                    .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
                    .map_err(|_| {
                        AdminGatewayError::Unexpected(anyhow!("No wallet module found"))
                    })?;
                let fee = wallet_module.send_fee().await.map_err(|e| {
                    AdminGatewayError::WithdrawError {
                        failure_reason: e.to_string(),
                    }
                })?;
                {
                    WithdrawDetails {
                        amount: btc_amount.into(),
                        mint_fees: None,
                        peg_out_fees: fee,
                    }
                }
            }
        };

        let total_cost = amount
            .checked_add(peg_out_fees.into())
            .and_then(|a| a.checked_add(mint_fees.unwrap_or(Amount::ZERO)))
            .ok_or_else(|| AdminGatewayError::Unexpected(anyhow!("Total cost overflow")))?;

        Ok(WithdrawPreviewResponse {
            withdraw_amount: amount,
            address: payload.address.assume_checked().to_string(),
            peg_out_fees,
            total_cost,
            mint_fees,
        })
    }

    /// Queries the client log for payment events and returns to the user.
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

    /// Returns a `BTreeMap` that is keyed by the `FederationId` and contains
    /// all the invite codes (with peer names) for the federation.
    async fn handle_export_invite_codes(
        &self,
    ) -> BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>> {
        let fed_manager = self.federation_manager.read().await;
        fed_manager.all_invite_codes().await
    }

    fn get_password_hash(&self) -> String {
        self.bcrypt_password_hash.clone()
    }

    fn gatewayd_version(&self) -> String {
        let gatewayd_version = env!("CARGO_PKG_VERSION");
        gatewayd_version.to_string()
    }

    async fn get_chain_source(&self) -> (ChainSource, Network) {
        (self.chain_source.clone(), self.network)
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

        let mut dbtx = self.gateway_db.begin_transaction_nc().await;
        let fed_config = dbtx.load_federation_config(*federation_id).await.ok_or(
            PublicGatewayError::FederationNotConnected(FederationNotConnected {
                federation_id_prefix: federation_id.to_prefix(),
            }),
        )?;

        let lightning_fee = fed_config.lightning_fee;
        let transaction_fee = fed_config.transaction_fee;

        // Convert gateway-common PaymentFee to lnv2-common PaymentFee for RoutingInfo
        let to_lnv2_fee = |fee: PaymentFee| -> fedimint_lnv2_common::gateway_api::PaymentFee {
            fedimint_lnv2_common::gateway_api::PaymentFee {
                base: fee.base,
                parts_per_million: fee.parts_per_million,
            }
        };

        Ok(self
            .public_key_v2(federation_id)
            .await
            .map(|module_public_key| RoutingInfo {
                lightning_public_key: context.lightning_public_key,
                lightning_alias: Some(context.lightning_alias.clone()),
                module_public_key,
                send_fee_default: to_lnv2_fee(lightning_fee + transaction_fee),
                // The base fee ensures that the gateway does not loose sats sending the payment due
                // to fees paid on the transaction claiming the outgoing contract or
                // subsequent transactions spending the newly issued ecash
                send_fee_minimum: to_lnv2_fee(transaction_fee),
                expiration_delta_default: 1440,
                expiration_delta_minimum: EXPIRATION_DELTA_MINIMUM_V2,
                // The base fee ensures that the gateway does not loose sats receiving the payment
                // due to fees paid on the transaction funding the incoming contract
                receive_fee: to_lnv2_fee(transaction_fee),
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

        if payload.contract.commitment.expiration <= duration_since_epoch().as_secs() {
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
        let ldk_client = self.get_ldk_client().await;

        let response = match description {
            Bolt11InvoiceDescription::Direct(description) => {
                ldk_client
                    .create_invoice(CreateInvoiceRequest {
                        payment_hash: Some(payment_hash),
                        amount_msat: amount.msats,
                        expiry_secs: expiry_time,
                        description: Some(InvoiceDescription::Direct(description)),
                    })
                    .await?
            }
            Bolt11InvoiceDescription::Hash(hash) => {
                ldk_client
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
    async fn complete_htlc(
        &self,
        htlc_response: fedimint_gateway_common::InterceptPaymentResponse,
    ) {
        loop {
            if let Some(ldk_client) = self.ldk_client.read().await.clone() {
                match ldk_client.complete_htlc(htlc_response.clone()).await {
                    Ok(..) => return,
                    Err(err) => {
                        warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failure trying to complete payment");
                    }
                }
            } else {
                warn!(target: LOG_GATEWAY, "LDK client not available, retrying...");
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
    ) -> std::result::Result<[u8; 32], fedimint_gateway_common::LightningRpcError> {
        let ldk_client = self
            .ldk_client
            .read()
            .await
            .clone()
            .ok_or(LightningRpcError::FailedToConnect)?;
        ldk_client
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

    async fn is_lnv1_invoice(&self, _invoice: &Bolt11Invoice) -> Option<Spanned<ClientHandleArc>> {
        // LDK does not support LNv1 invoices
        None
    }

    async fn relay_lnv1_swap(
        &self,
        _client: &ClientHandleArc,
        _invoice: &Bolt11Invoice,
    ) -> anyhow::Result<FinalReceiveState> {
        // LDK does not support LNv1 swaps
        Err(anyhow!("LNv1 swaps are not supported"))
    }
}
