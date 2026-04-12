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
pub mod db;
pub mod envs;
mod error;
mod events;
mod federation_manager;
pub mod rpc;

use std::collections::BTreeMap;
use std::env;
use std::fmt::Display;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::anyhow;
use async_trait::async_trait;
use bitcoin::hashes::{Hash, sha256};
use bitcoin::{Address, Network, OutPoint, secp256k1};
use clap::Parser;
use client::GatewayClientBuilder;
pub use config::GatewayParameters;
use config::{DatabaseBackend, GatewayOpts};
use envs::FM_GATEWAY_SKIP_WAIT_FOR_SYNC_ENV;
use error::FederationNotConnected;
use federation_manager::FederationManager;
use fedimint_bip39::{Bip39RootSecretStrategy, Language, Mnemonic};
use fedimint_bitcoind::bitcoincore::BitcoindClient;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_client::{Client, ClientHandleArc};
use fedimint_core::config::FederationId;
use fedimint_core::core::OperationId;
use fedimint_core::db::{Database, DatabaseTransaction};
use fedimint_core::envs::is_env_var_set;
use fedimint_core::module::CommonModuleInit;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::rustls::install_crypto_provider;
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::task::{TaskGroup, TaskShutdownToken, block_in_place};
use fedimint_core::time::duration_since_epoch;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow, SafeUrl, Spanned};
use fedimint_core::{Amount, BitcoinAmountOrAll, crit, fedimint_build_code_version_env};
use fedimint_gateway_common::{
    ChainSource, InterceptPaymentRequest, LightningContext, LightningRpcError, PaymentAction,
    PaymentFee, Preimage, RegisteredProtocol, V1_API_ENDPOINT, WithdrawResponse,
};
use fedimint_gwv2_client::{
    EXPIRATION_DELTA_MINIMUM_V2, FinalReceiveState, GatewayClientModuleV2, IGatewayClientV2,
};
use fedimint_lnurl::VerifyResponse;
use fedimint_lnv2_common::Bolt11InvoiceDescription;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::{
    CreateBolt11InvoicePayload, RoutingInfo, SendPaymentPayload,
};
use fedimint_logging::{LOG_GATEWAY, LOG_LIGHTNING};
use fedimint_mintv2_client::MintClientInit;
use ldk_node::lightning::ln::msgs::SocketAddress;
use ldk_node::lightning::routing::gossip::NodeAlias;
use ldk_node::payment::{PaymentKind, PaymentStatus, SendingParameters};
use lightning::ln::channelmanager::PaymentId;
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning_invoice::{
    Bolt11Invoice, Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description,
};
use rand::rngs::OsRng;
use tokio::sync::{RwLock, oneshot};
use tracing::{debug, info, info_span, warn};

use crate::db::GatewayDbtxNcExt as _;
use crate::envs::FM_GATEWAY_MNEMONIC_ENV;
use crate::error::CliError;
use crate::rpc::run_webserver;

/// Default Bitcoin network for testing purposes.
pub const DEFAULT_NETWORK: Network = Network::Regtest;

pub type Result<T> = std::result::Result<T, CliError>;
pub type AdminResult<T> = std::result::Result<T, CliError>;

/// Name of the gateway's database that is used for metadata and configuration
/// storage.
const DB_FILE: &str = "gatewayd.db";

/// Name of the folder that the gateway uses to store its node database when
/// running in LDK mode.
const LDK_NODE_DB_FOLDER: &str = "ldk_node";

/// Simplified gateway state: the node is always available, so we only
/// track whether we are running normally or shutting down.
#[derive(Clone, Debug)]
pub enum GatewayState {
    Running,
    ShuttingDown,
}

impl Display for GatewayState {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            GatewayState::Running => write!(f, "Running"),
            GatewayState::ShuttingDown => write!(f, "ShuttingDown"),
        }
    }
}

/// Helper struct for storing the registration parameters for LNv1 for each
/// network protocol.
#[derive(Debug, Clone)]
pub(crate) struct Registration {
    /// The url to advertise in the registration that clients can use to connect
    pub(crate) endpoint_url: SafeUrl,

    /// Keypair that was used to register the gateway registration
    pub(crate) keypair: secp256k1::Keypair,
}

impl Registration {
    pub async fn new(db: &Database, endpoint_url: SafeUrl, protocol: RegisteredProtocol) -> Self {
        let keypair = AppState::load_or_create_gateway_keypair(db, protocol).await;
        Self {
            endpoint_url,
            keypair,
        }
    }
}

#[bon::bon]
impl AppState {
    /// Construct an [`AppState`] using a fluent builder API.
    ///
    /// # Example
    /// ```ignore
    /// let state = AppState::builder(client_builder, gateway_db)
    ///     .listen(addr)
    ///     .api_addr(url)
    ///     .network(Network::Regtest)
    ///     .node(node)
    ///     .chain_source(chain_source)
    ///     .build()
    ///     .await?;
    /// ```
    #[builder(start_fn = builder, finish_fn = build)]
    pub async fn new_with_builder(
        #[builder(start_fn)] client_builder: GatewayClientBuilder,
        #[builder(start_fn)] gateway_db: Database,
        node: Arc<ldk_node::Node>,
        chain_source: ChainSource,
        #[builder(default = ([127, 0, 0, 1], 80).into())] listen: SocketAddr,
        api_addr: Option<SafeUrl>,
        #[builder(default = DEFAULT_NETWORK)] network: Network,
        #[builder(default = PaymentFee::TRANSACTION_FEE_DEFAULT)] default_routing_fees: PaymentFee,
        #[builder(default = PaymentFee::TRANSACTION_FEE_DEFAULT)]
        default_transaction_fees: PaymentFee,
    ) -> anyhow::Result<AppState> {
        let versioned_api = api_addr.map(|addr| {
            addr.join(V1_API_ENDPOINT)
                .expect("Failed to version gateway API address")
        });

        AppState::new(
            GatewayParameters {
                listen,
                versioned_api,
                network,
                default_routing_fees,
                default_transaction_fees,
            },
            gateway_db,
            client_builder,
            node,
            chain_source,
        )
        .await
    }
}

#[derive(Clone)]
pub struct AppState {
    /// The gateway's federation manager.
    pub(crate) federation_manager: Arc<RwLock<FederationManager>>,

    /// The underlying LDK lightning node, always available.
    pub(crate) node: Arc<ldk_node::Node>,

    /// The current state of the Gateway.
    pub(crate) state: Arc<RwLock<GatewayState>>,

    /// Builder struct that allows the gateway to build a Fedimint client, which
    /// handles the communication with a federation.
    pub(crate) client_builder: GatewayClientBuilder,

    /// Database for Gateway metadata.
    pub(crate) gateway_db: Database,

    /// The socket the gateway listens on.
    pub(crate) listen: SocketAddr,

    /// The task group for all tasks related to the gateway.
    pub(crate) task_group: TaskGroup,

    /// The Bitcoin network that the Lightning network is configured to.
    pub(crate) network: Network,

    /// The source of the Bitcoin blockchain data
    pub(crate) chain_source: ChainSource,

    /// The default routing fees for new federations
    pub(crate) default_routing_fees: PaymentFee,

    /// The default transaction fees for new federations
    pub(crate) default_transaction_fees: PaymentFee,

    /// A map of the network protocols the gateway supports to the data needed
    /// for registering with a federation.
    pub(crate) registrations: BTreeMap<RegisteredProtocol, Registration>,

    /// Lock pool used to ensure that `pay` doesn't allow for multiple
    /// simultaneous calls with the same invoice to execute in parallel.
    pub(crate) outbound_lightning_payment_lock_pool: Arc<lockable::LockPool<PaymentId>>,

    /// Lock pool used to ensure that `pay_offer` doesn't allow for multiple
    /// simultaneous calls with the same offer to execute in parallel.

    /// A map keyed by the `UserChannelId` of a channel that is currently
    /// opening. The `Sender` is used to communicate the `OutPoint` back to
    /// the API handler from the event handler when the channel has been
    /// opened and is now pending.
    pub(crate) pending_channels:
        Arc<RwLock<BTreeMap<UserChannelId, oneshot::Sender<anyhow::Result<OutPoint>>>>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("federation_manager", &self.federation_manager)
            .field("state", &self.state)
            .field("client_builder", &self.client_builder)
            .field("gateway_db", &self.gateway_db)
            .field("listen", &self.listen)
            .field("node_id", &self.node.node_id())
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
pub(crate) async fn withdraw_v2(
    client: &ClientHandleArc,
    wallet_module: &fedimint_walletv2_client::WalletClientModule,
    address: &Address,
    amount: BitcoinAmountOrAll,
) -> AdminResult<WithdrawResponse> {
    let fee = wallet_module
        .send_fee()
        .await
        .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?;

    let withdraw_amount = match amount {
        BitcoinAmountOrAll::All => {
            let balance = bitcoin::Amount::from_sat(
                client
                    .get_balance_for_btc()
                    .await
                    .map_err(|err| {
                        CliError::internal(format!(
                            "Balance not available: {}",
                            err.fmt_compact_anyhow()
                        ))
                    })?
                    .msats
                    / 1000,
            );
            balance.checked_sub(fee).ok_or_else(|| {
                CliError::internal(format!("Insufficient funds. Balance: {balance} Fee: {fee}"))
            })?
        }
        BitcoinAmountOrAll::Amount(a) => a,
    };

    let operation_id = wallet_module
        .send(address.as_unchecked().clone(), withdraw_amount, Some(fee))
        .await
        .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?;

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
            Err(CliError::internal("Withdrawal transaction was aborted"))
        }
        fedimint_walletv2_client::FinalSendOperationState::Failure => {
            Err(CliError::internal("Withdrawal failed"))
        }
    }
}

/// Calculates an estimated max withdrawable amount on-chain
async fn calculate_max_withdrawable(
    client: &ClientHandleArc,
    _address: &Address,
) -> AdminResult<WithdrawDetails> {
    let balance = client.get_balance_for_btc().await.map_err(|err| {
        CliError::internal(format!(
            "Balance not available: {}",
            err.fmt_compact_anyhow()
        ))
    })?;

    let wallet_module = client
        .get_first_module::<fedimint_walletv2_client::WalletClientModule>()
        .map_err(|_| CliError::internal("No wallet module found"))?;

    let peg_out_fees = wallet_module
        .send_fee()
        .await
        .map_err(|e| CliError::internal(format!("Withdraw error: {e}")))?;

    let max_withdrawable_before_mint_fees = balance
        .checked_sub(peg_out_fees.into())
        .ok_or_else(|| CliError::internal("Insufficient balance to cover peg-out fees"))?;

    let mint_fees = Amount::ZERO;

    let max_withdrawable = max_withdrawable_before_mint_fees.saturating_sub(mint_fees);

    Ok(WithdrawDetails {
        amount: max_withdrawable,
        mint_fees: Some(mint_fees),
        peg_out_fees,
    })
}

impl AppState {
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
    pub async fn new_with_default_modules() -> anyhow::Result<AppState> {
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
        registry.attach(fedimint_walletv2_client::WalletClientInit);

        let client_builder =
            GatewayClientBuilder::new(opts.data_dir.clone(), registry, opts.db_backend).await?;

        if Self::load_mnemonic(&gateway_db).await.is_none() {
            let mnemonic = if let Ok(words) = std::env::var(FM_GATEWAY_MNEMONIC_ENV) {
                info!(target: LOG_GATEWAY, "Using provided mnemonic from environment variable");
                Mnemonic::parse_in_normalized(Language::English, words.as_str())
                    .map_err(|e| anyhow!("Seed phrase provided in environment was invalid {e:?}"))?
            } else {
                debug!(target: LOG_GATEWAY, "Generating mnemonic and writing entropy to client storage");
                Bip39RootSecretStrategy::<12>::random(&mut OsRng)
            };

            Client::store_encodable_client_secret(&gateway_db, mnemonic.to_entropy())
                .await
                .map_err(|e| anyhow!("Mnemonic error: {e}"))?;
        }

        let mnemonic = Self::load_mnemonic(&gateway_db)
            .await
            .expect("mnemonic should be set");

        let lightning_port = env::var("FM_PORT_LDK")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(9735);
        let alias = env::var("FM_LDK_ALIAS").unwrap_or_default();

        let runtime = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .expect("Failed to build LDK runtime"),
        );

        let node = create_ldk_node(
            &client_builder.data_dir().join(LDK_NODE_DB_FOLDER),
            chain_source.clone(),
            gateway_parameters.network,
            lightning_port,
            alias,
            mnemonic,
            runtime,
        )?;
        let node = Arc::new(node);

        info!(
            target: LOG_GATEWAY,
            version = %fedimint_build_code_version_env!(),
            "Starting gatewayd",
        );

        AppState::new(
            gateway_parameters,
            gateway_db,
            client_builder,
            node,
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
        node: Arc<ldk_node::Node>,
        chain_source: ChainSource,
    ) -> anyhow::Result<AppState> {
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
            node,
            state: Arc::new(RwLock::new(GatewayState::Running)),
            client_builder,
            gateway_db: gateway_db.clone(),
            listen: gateway_parameters.listen,
            task_group,
            network,
            chain_source,
            default_routing_fees: gateway_parameters.default_routing_fees,
            default_transaction_fees: gateway_parameters.default_transaction_fees,
            registrations,
            outbound_lightning_payment_lock_pool: Arc::new(lockable::LockPool::new()),
            pending_channels: Arc::new(RwLock::new(BTreeMap::new())),
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
        _runtime: Arc<tokio::runtime::Runtime>,
    ) -> anyhow::Result<TaskShutdownToken> {
        install_crypto_provider().await;

        // Wait for chain sync before loading clients
        if !is_env_var_set(FM_GATEWAY_SKIP_WAIT_FOR_SYNC_ENV) {
            self.wait_for_chain_sync().await?;
        }

        self.load_clients().await?;
        self.start_ldk_event_loop();
        // start webserver last to avoid handling requests before fully initialized
        let handle = self.task_group.make_handle();
        run_webserver(self.clone()).await?;
        let shutdown_receiver = handle.make_shutdown_rx();
        Ok(shutdown_receiver)
    }

    /// Starts the LDK event loop that processes incoming lightning events.
    fn start_ldk_event_loop(&self) {
        let gateway = self.clone();
        self.task_group.spawn("ldk-events", |handle| async move {
            loop {
                let event = tokio::select! {
                    event = gateway.node.next_event_async() => event,
                    () = handle.make_shutdown_rx() => break,
                };

                gateway.process_ldk_event(event).await;

                if let Err(e) = gateway.node.event_handled() {
                    warn!(
                        target: LOG_LIGHTNING,
                        err = %e.fmt_compact(),
                        "Failed to mark event handled",
                    );
                }
            }
        });
    }

    /// Processes a single LDK event.
    async fn process_ldk_event(&self, event: ldk_node::Event) {
        match event {
            ldk_node::Event::PaymentClaimable {
                payment_id: _,
                payment_hash,
                claimable_amount_msat,
                claim_deadline,
                custom_records: _,
            } => {
                let hash = Hash::from_slice(&payment_hash.0).expect("Failed to create Hash");

                let payment_request = InterceptPaymentRequest {
                    payment_hash: hash,
                    amount_msat: claimable_amount_msat,
                    expiry: claim_deadline.unwrap_or_default(),
                    short_channel_id: None,
                    incoming_chan_id: 0,
                    htlc_id: 0,
                };

                self.handle_lightning_payment(payment_request).await;
            }
            ldk_node::Event::ChannelPending {
                channel_id,
                user_channel_id,
                former_temporary_channel_id: _,
                counterparty_node_id: _,
                funding_txo,
            } => {
                info!(
                    target: LOG_LIGHTNING,
                    %channel_id,
                    "LDK Channel is pending",
                );
                let mut channels = self.pending_channels.write().await;
                if let Some(sender) = channels.remove(&UserChannelId(user_channel_id)) {
                    let _ = sender.send(Ok(funding_txo));
                } else {
                    debug!(
                        ?user_channel_id,
                        "No channel pending channel open for user channel id",
                    );
                }
            }
            ldk_node::Event::ChannelClosed {
                channel_id,
                user_channel_id,
                counterparty_node_id: _,
                reason,
            } => {
                info!(
                    target: LOG_LIGHTNING,
                    %channel_id,
                    "LDK Channel is closed",
                );
                let mut channels = self.pending_channels.write().await;
                if let Some(sender) = channels.remove(&UserChannelId(user_channel_id)) {
                    let reason = if let Some(reason) = reason {
                        reason.to_string()
                    } else {
                        "Channel has been closed".to_string()
                    };
                    let _ = sender.send(Err(anyhow::anyhow!(reason)));
                } else {
                    debug!(
                        ?user_channel_id,
                        "No channel pending channel open for user channel id",
                    );
                }
            }
            _ => {}
        }
    }

    /// Handles an intercepted lightning payment. If the payment is part of an
    /// incoming payment to a federation, spawns a state machine and hands the
    /// payment off to it. Otherwise, fails the HTLC since forwarding is not
    /// supported.
    pub async fn handle_lightning_payment(&self, payment_request: InterceptPaymentRequest) {
        info!(
            target: LOG_GATEWAY,
            payment_hash = %payment_request.payment_hash,
            amount_msat = %payment_request.amount_msat,
            "Intercepting lightning payment",
        );

        let lnv2_result = self
            .try_handle_lightning_payment_lnv2(&payment_request)
            .await;
        if lnv2_result.is_ok() {
            return;
        }

        // No matching federation contract; fail the HTLC
        let ph = PaymentHash(*payment_request.payment_hash.as_byte_array());
        if let Err(err) = self.node.bolt11_payment().fail_for_hash(ph) {
            warn!(
                target: LOG_GATEWAY,
                err = %err.fmt_compact(),
                "Error failing unmatched HTLC",
            );
        }
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

            let ph = PaymentHash(*htlc_request.payment_hash.as_byte_array());
            if let Err(err) = self.node.bolt11_payment().fail_for_hash(ph) {
                warn!(
                    target: LOG_GATEWAY,
                    err = %err.fmt_compact(),
                    "Error failing HTLC after relay error",
                );
            }
        }

        Ok(())
    }

    /// Waits for the Lightning node to be synced to the Bitcoin blockchain.
    async fn wait_for_chain_sync(&self) -> anyhow::Result<()> {
        use fedimint_core::envs::{FM_IN_DEVIMINT_ENV, is_env_var_set};
        use fedimint_core::util::{backoff_util, retry};

        // In devimint, we explicitly sync the onchain wallet to start the sync quicker
        if is_env_var_set(FM_IN_DEVIMINT_ENV) {
            block_in_place(|| {
                let _ = self.node.sync_wallets();
            });
        }

        retry(
            "Wait for chain sync",
            backoff_util::background_backoff(),
            || async {
                let node_status = self.node.status();
                let block_height = node_status.current_best_block.height;
                if node_status.latest_lightning_wallet_sync_timestamp.is_some() {
                    Ok(())
                } else {
                    warn!(
                        target: LOG_LIGHTNING,
                        block_height = %block_height,
                        "Lightning node is not synced yet",
                    );
                    Err(anyhow::anyhow!("Not synced yet"))
                }
            },
        )
        .await?;

        info!(target: LOG_LIGHTNING, "Gateway successfully synced with the chain");
        Ok(())
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

    pub(crate) async fn load_mnemonic(gateway_db: &Database) -> Option<Mnemonic> {
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
    /// Verifies that the federation has an LNv2 lightning module and that the
    /// network matches the gateway's network.
    pub(crate) async fn check_federation_network(
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
            return Err(CliError::internal(format!(
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
                return Err(CliError::internal(format!(
                    "Unsupported LNv2 network {}",
                    ln_cfg.network
                )));
            }
        }

        Ok(())
    }

    /// Returns the `LightningContext` built from the node's current state.
    pub async fn get_lightning_context(
        &self,
    ) -> std::result::Result<LightningContext, LightningRpcError> {
        let alias = match self.node.node_alias() {
            Some(alias) => alias.to_string(),
            None => format!("LDK Fedimint Gateway Node {}", self.node.node_id()),
        };

        Ok(LightningContext {
            lightning_public_key: self.node.node_id(),
            lightning_alias: alias,
            lightning_network: self.node.config().network,
            supports_private_payments: false,
        })
    }

    /// Iterates through all of the federations the gateway is registered with
    /// and requests to remove the registration record.
    /// This is a no-op for LDK since it does not use LNv1 registration.
    pub async fn unannounce_from_all_federations(&self) {
        // LDK does not use LNv1 gateway registration
    }
}

// LNv2 Gateway implementation
impl AppState {
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
        let fed_config =
            dbtx.load_federation_config(*federation_id)
                .await
                .ok_or(CliError::bad_request(FederationNotConnected {
                    federation_id_prefix: federation_id.to_prefix(),
                }))?;

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
            .map_err(|e| CliError::internal(format!("LNv2 outgoing payment error: {e}")))
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
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "The contract is invalid".to_string(),
            )));
        }

        let payment_info =
            self.routing_info_v2(&payload.federation_id)
                .await?
                .ok_or(CliError::internal(format!(
                    "LNv2 incoming payment error: Federation {} does not exist",
                    payload.federation_id
                )))?;

        if payload.contract.commitment.refund_pk != payment_info.module_public_key {
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "The incoming contract is keyed to another gateway".to_string(),
            )));
        }

        let contract_amount = payment_info.receive_fee.subtract_from(payload.amount.msats);

        if contract_amount == Amount::ZERO {
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "Zero amount incoming contracts are not supported".to_string(),
            )));
        }

        if contract_amount != payload.contract.commitment.amount {
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "The contract amount does not pay the correct amount of fees".to_string(),
            )));
        }

        if payload.contract.commitment.expiration <= duration_since_epoch().as_secs() {
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "The contract has already expired".to_string(),
            )));
        }

        let payment_hash = match payload.contract.commitment.payment_image {
            PaymentImage::Hash(payment_hash) => payment_hash,
            PaymentImage::Point(..) => {
                return Err(CliError::internal(format!(
                    "LNv2 incoming payment error: {}",
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
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "PaymentHash is already registered".to_string(),
            )));
        }

        dbtx.commit_tx_result().await.map_err(|_| {
            CliError::internal(format!(
                "LNv2 incoming payment error: {}",
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
        let ph = PaymentHash(*payment_hash.as_byte_array());

        let ldk_description = match description {
            Bolt11InvoiceDescription::Direct(desc) => {
                LdkBolt11InvoiceDescription::Direct(Description::new(desc).map_err(|_| {
                    LightningRpcError::FailedToGetInvoice {
                        failure_reason: "Invalid description".to_string(),
                    }
                })?)
            }
            Bolt11InvoiceDescription::Hash(hash) => {
                LdkBolt11InvoiceDescription::Hash(lightning_invoice::Sha256(hash))
            }
        };

        self.node
            .bolt11_payment()
            .receive_for_hash(amount.msats, &ldk_description, expiry_time, ph)
            .map_err(|e| LightningRpcError::FailedToGetInvoice {
                failure_reason: e.to_string(),
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
            .ok_or(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
                "No corresponding decryption contract available".to_string(),
            )))?;

        if registered_incoming_contract.incoming_amount_msats != amount_msats {
            return Err(CliError::internal(format!(
                "LNv2 incoming payment error: {}",
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
impl IGatewayClientV2 for AppState {
    async fn complete_htlc(
        &self,
        htlc_response: fedimint_gateway_common::InterceptPaymentResponse,
    ) {
        let ph = PaymentHash(*htlc_response.payment_hash.as_byte_array());

        // TODO: Get the actual amount from the LDK node. This value is only used by
        // `ldk-node` to ensure that the amount claimed isn't less than the amount
        // expected, but we've already verified that the amount is correct when we
        // intercepted the payment.
        let claimable_amount_msat = 999_999_999_999_999;

        let ph_hex_str = hex::encode(htlc_response.payment_hash);

        if let PaymentAction::Settle(preimage) = htlc_response.action {
            if let Err(err) = self.node.bolt11_payment().claim_for_hash(
                ph,
                claimable_amount_msat,
                PaymentPreimage(preimage.0),
            ) {
                warn!(
                    target: LOG_GATEWAY,
                    payment_hash = %ph_hex_str,
                    err = %err.fmt_compact(),
                    "Failed to claim LDK payment",
                );
            }
        } else {
            warn!(
                target: LOG_GATEWAY,
                payment_hash = %ph_hex_str,
                "Unwinding payment because the action was not Settle",
            );
            if let Err(err) = self.node.bolt11_payment().fail_for_hash(ph) {
                warn!(
                    target: LOG_GATEWAY,
                    payment_hash = %ph_hex_str,
                    err = %err.fmt_compact(),
                    "Failed to unwind LDK payment",
                );
            }
        }
    }

    async fn is_direct_swap(
        &self,
        invoice: &Bolt11Invoice,
    ) -> anyhow::Result<Option<(IncomingContract, ClientHandleArc)>> {
        if self.node.node_id() == invoice.get_payee_pub_key() {
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
        let payment_id = PaymentId(*invoice.payment_hash().as_byte_array());

        // Lock by the payment hash to prevent multiple simultaneous calls with the same
        // invoice from executing.
        let _payment_lock_guard = self
            .outbound_lightning_payment_lock_pool
            .async_lock(payment_id)
            .await;

        if self.node.payment(&payment_id).is_none() {
            assert_eq!(
                self.node
                    .bolt11_payment()
                    .send(
                        &invoice,
                        Some(SendingParameters {
                            max_total_routing_fee_msat: Some(Some(max_fee.msats)),
                            max_total_cltv_expiry_delta: Some(max_delay as u32),
                            max_path_count: None,
                            max_channel_saturation_power_of_half: None,
                        }),
                    )
                    .map_err(|e| LightningRpcError::FailedPayment {
                        failure_reason: format!("LDK payment failed to initialize: {e:?}"),
                    })?,
                payment_id
            );
        }

        loop {
            if let Some(payment_details) = self.node.payment(&payment_id) {
                match payment_details.status {
                    PaymentStatus::Pending => {}
                    PaymentStatus::Succeeded => {
                        if let PaymentKind::Bolt11 {
                            preimage: Some(preimage),
                            ..
                        } = payment_details.kind
                        {
                            return Ok(preimage.0);
                        }
                    }
                    PaymentStatus::Failed => {
                        return Err(LightningRpcError::FailedPayment {
                            failure_reason: "LDK payment failed".to_string(),
                        });
                    }
                }
            }
            fedimint_core::runtime::sleep(Duration::from_millis(100)).await;
        }
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

/// Creates an LDK node instance from the given configuration parameters.
pub fn create_ldk_node(
    data_dir: &Path,
    chain_source: ChainSource,
    network: Network,
    lightning_port: u16,
    alias: String,
    mnemonic: Mnemonic,
    runtime: Arc<tokio::runtime::Runtime>,
) -> anyhow::Result<ldk_node::Node> {
    let mut bytes = [0u8; 32];
    let alias = if alias.is_empty() {
        "LDK Gateway".to_string()
    } else {
        alias
    };
    let alias_bytes = alias.as_bytes();
    let truncated = &alias_bytes[..alias_bytes.len().min(32)];
    bytes[..truncated.len()].copy_from_slice(truncated);
    let node_alias = Some(NodeAlias(bytes));

    let mut node_builder = ldk_node::Builder::from_config(ldk_node::config::Config {
        network,
        listening_addresses: Some(vec![SocketAddress::TcpIpV4 {
            addr: [0, 0, 0, 0],
            port: lightning_port,
        }]),
        node_alias,
        ..Default::default()
    });

    node_builder.set_entropy_bip39_mnemonic(mnemonic, None);

    match chain_source {
        ChainSource::Bitcoind {
            username,
            password,
            server_url,
        } => {
            node_builder.set_chain_source_bitcoind_rpc(
                server_url
                    .host_str()
                    .expect("Could not retrieve host from bitcoind RPC url")
                    .to_string(),
                server_url
                    .port()
                    .expect("Could not retrieve port from bitcoind RPC url"),
                username,
                password,
            );
        }
        ChainSource::Esplora { server_url } => {
            node_builder.set_chain_source_esplora(get_esplora_url(server_url)?, None);
        }
    }
    let Some(data_dir_str) = data_dir.to_str() else {
        return Err(anyhow::anyhow!("Invalid data dir path"));
    };
    node_builder.set_storage_dir_path(data_dir_str.to_string());

    info!(
        target: LOG_LIGHTNING,
        data_dir = %data_dir_str,
        alias = %alias,
        "Starting LDK Node...",
    );
    let node = node_builder.build()?;
    node.start_with_runtime(runtime).map_err(|err| {
        crit!(
            target: LOG_LIGHTNING,
            err = %err.fmt_compact(),
            "Failed to start LDK Node",
        );
        anyhow::anyhow!("Failed to start LDK Node: {err}")
    })?;

    info!("Successfully started LDK Node");
    Ok(node)
}

/// Maps LDK's `PaymentKind` to an optional preimage and an optional payment
/// hash depending on the type of payment.
pub(crate) fn get_preimage_and_payment_hash(
    kind: &PaymentKind,
) -> (
    Option<Preimage>,
    Option<sha256::Hash>,
    fedimint_gateway_common::PaymentKind,
) {
    match kind {
        PaymentKind::Bolt11 {
            hash,
            preimage,
            secret: _,
        } => (
            preimage.map(|p| Preimage(p.0)),
            Some(sha256::Hash::from_slice(&hash.0).expect("Failed to convert payment hash")),
            fedimint_gateway_common::PaymentKind::Bolt11,
        ),
        PaymentKind::Bolt11Jit {
            hash,
            preimage,
            secret: _,
            lsp_fee_limits: _,
            ..
        } => (
            preimage.map(|p| Preimage(p.0)),
            Some(sha256::Hash::from_slice(&hash.0).expect("Failed to convert payment hash")),
            fedimint_gateway_common::PaymentKind::Bolt11,
        ),
        PaymentKind::Bolt12Offer {
            hash,
            preimage,
            secret: _,
            offer_id: _,
            payer_note: _,
            quantity: _,
        } => (
            preimage.map(|p| Preimage(p.0)),
            hash.map(|h| sha256::Hash::from_slice(&h.0).expect("Failed to convert payment hash")),
            fedimint_gateway_common::PaymentKind::Bolt12Offer,
        ),
        PaymentKind::Bolt12Refund {
            hash,
            preimage,
            secret: _,
            payer_note: _,
            quantity: _,
        } => (
            preimage.map(|p| Preimage(p.0)),
            hash.map(|h| sha256::Hash::from_slice(&h.0).expect("Failed to convert payment hash")),
            fedimint_gateway_common::PaymentKind::Bolt12Refund,
        ),
        PaymentKind::Spontaneous { hash, preimage } => (
            preimage.map(|p| Preimage(p.0)),
            Some(sha256::Hash::from_slice(&hash.0).expect("Failed to convert payment hash")),
            fedimint_gateway_common::PaymentKind::Bolt11,
        ),
        PaymentKind::Onchain { .. } => (None, None, fedimint_gateway_common::PaymentKind::Onchain),
    }
}

/// When a port is specified in the Esplora URL, the esplora client inside LDK
/// node cannot connect to the lightning node when there is a trailing slash.
fn get_esplora_url(server_url: SafeUrl) -> anyhow::Result<String> {
    let host = server_url
        .host_str()
        .ok_or(anyhow::anyhow!("Missing esplora host"))?;
    let server_url = if let Some(port) = server_url.port() {
        format!("{}://{}:{}", server_url.scheme(), host, port)
    } else {
        server_url.to_string()
    };
    Ok(server_url)
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct UserChannelId(pub ldk_node::UserChannelId);

impl PartialOrd for UserChannelId {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for UserChannelId {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.0.cmp(&other.0.0)
    }
}
