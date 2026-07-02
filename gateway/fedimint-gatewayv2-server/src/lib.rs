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
pub mod rpc_server;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use ::lightning::ln::channelmanager::PaymentId;
use anyhow::{Context, anyhow, bail, ensure};
use async_trait::async_trait;
use bitcoin::address::NetworkUnchecked;
use bitcoin::hashes::{Hash, sha256};
use bitcoin::{Address, Network, OutPoint, Txid};
pub use client::GatewayClientBuilder;
pub use config::GatewayOpts;
use federation_manager::FederationManager;
use fedimint_bip39::{Bip39RootSecretStrategy, Mnemonic};
use fedimint_client::ClientHandleArc;
use fedimint_client::module_init::ClientModuleInitRegistry;
use fedimint_client::secret::RootSecretStrategy;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::{ModuleKind, OperationId};
use fedimint_core::db::Database;
use fedimint_core::encoding::Encodable as _;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::ModuleDecoderRegistry;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::task::sleep;
use fedimint_core::time::duration_since_epoch;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow, Spanned};
use fedimint_core::{Amount, get_network_for_address};
use fedimint_gateway_common::{
    ChainSource, CloseChannelsWithPeerRequest, CloseChannelsWithPeerResponse,
    CreateInvoiceForOperatorPayload, DepositAddressPayload, GatewayFedConfig, LeaveFedPayload,
    OpenChannelRequest, PayInvoiceForOperatorPayload, ReceiveEcashPayload, ReceiveEcashResponse,
    SendOnchainRequest, SpendEcashPayload, SpendEcashResponse, WithdrawResponse,
};
use fedimint_gatewayv2_cli_core as cli;
use fedimint_gwv2_client::api::GatewayFederationApi as _;
use fedimint_gwv2_client::events::{
    IncomingPaymentFailed, IncomingPaymentSucceeded, OutgoingPaymentFailed,
    OutgoingPaymentSucceeded,
};
use fedimint_gwv2_client::{
    EXPIRATION_DELTA_MINIMUM_V2, FinalReceiveState, GatewayClientModuleV2, IGatewayClientV2,
};
use fedimint_lightning::{InterceptPaymentResponse, LightningRpcError, Preimage};
use fedimint_lnurl::VerifyResponse;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::{
    CreateBolt11InvoicePayload, PaymentFee, RoutingInfo, SendPaymentPayload,
};
use fedimint_lnv2_common::{Bolt11InvoiceDescription, LightningInvoice};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::{
    MintClientInit as MintV2ClientInit, MintClientModule as MintV2ClientModule,
};
use fedimint_wallet_common::PegOutFees;
use futures::StreamExt as _;
use lightning_invoice::Bolt11Invoice;
use rand::rngs::OsRng;
use tokio::sync::{RwLock, oneshot};
use tracing::{debug, info, info_span, warn};

pub use crate::cli_server::run_cli;
use crate::db::{GatewayDbtxNcExt as _, IncomingContractRow, OutgoingContractRow};
use crate::lightning::ldk::UserChannelId;
pub use crate::lightning::ldk::build_ldk_node;
pub use crate::rpc_server::run_public;

/// Default Bitcoin network for testing purposes.
pub const DEFAULT_NETWORK: Network = Network::Regtest;

pub type Result<T> = anyhow::Result<T>;
pub type AdminResult<T> = anyhow::Result<T>;

/// Decodes an event-log entry into a specific gateway event, gating on the
/// entry's `kind`/`module` first. `PersistedLogEntry::to_event` only attempts a
/// JSON deserialization, so without this gate a sibling event with a
/// compatible shape could decode by accident.
fn as_gw_event<E: fedimint_eventlog::Event>(
    entry: &fedimint_eventlog::PersistedLogEntry,
) -> Option<E> {
    (entry.kind == E::KIND && entry.module_kind() == E::MODULE.as_ref())
        .then(|| entry.to_event::<E>())
        .flatten()
}

/// Name of the gateway's database that is used for metadata and configuration
/// storage.
pub const DB_FILE: &str = "gatewayd.db";

/// Name of the folder that the gateway uses to store its node database when
/// running in LDK mode.
pub const LDK_NODE_DB_FOLDER: &str = "ldk_node";

/// The shared, cheaply-cloneable gateway state. Constructed in `main` and
/// handed to every long-running task (webserver, admin CLI, LDK event loop);
/// modeled on picomint's `AppState`.
#[derive(Clone)]
pub struct Gateway {
    /// The gateway's federation manager.
    federation_manager: Arc<RwLock<FederationManager>>,

    /// The gateway's LDK lightning node, built in `main` before any task is
    /// spawned (see [`build_ldk_node`]).
    node: Arc<ldk_node::Node>,

    /// Serializes concurrent [`Gateway::pay`] calls for the same invoice so the
    /// (non-idempotent) LDK send runs at most once per payment hash.
    outbound_lightning_payment_lock_pool: Arc<lockable::LockPool<PaymentId>>,

    /// Channels currently being opened, keyed by `UserChannelId`. The LDK event
    /// loop uses the sender to hand the funding outpoint back to the waiting
    /// `open_channel` caller.
    pending_channels:
        Arc<RwLock<BTreeMap<UserChannelId, oneshot::Sender<anyhow::Result<OutPoint>>>>>,

    /// Builder struct that allows the gateway to build a Fedimint client, which
    /// handles the communication with a federation.
    client_builder: GatewayClientBuilder,

    /// Database for Gateway metadata.
    gateway_db: Database,

    /// The socket the gateway's API webserver listens on.
    listen: SocketAddr,

    /// The Bitcoin network that the Lightning network is configured to.
    network: Network,

    /// The default routing fees for new federations
    default_routing_fees: PaymentFee,

    /// The default transaction fees for new federations
    default_transaction_fees: PaymentFee,
}

impl std::fmt::Debug for Gateway {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Gateway")
            .field("federation_manager", &self.federation_manager)
            .field("client_builder", &self.client_builder)
            .field("gateway_db", &self.gateway_db)
            .field("listen", &self.listen)
            .finish_non_exhaustive()
    }
}

/// Executes a withdrawal using the walletv2 module
async fn withdraw_v2(
    wallet_module: &fedimint_walletv2_client::WalletClientModule,
    address: &Address,
    withdraw_amount: bitcoin::Amount,
) -> AdminResult<WithdrawResponse> {
    let fee = wallet_module
        .send_fee()
        .await
        .map_err(|e| anyhow!("Error withdrawing funds onchain: {e}"))?;

    let operation_id = wallet_module
        .send(
            address.as_unchecked().clone(),
            withdraw_amount,
            Some(fee),
            serde_json::Value::Null,
        )
        .await
        .map_err(|e| anyhow!("Error withdrawing funds onchain: {e}"))?;

    let result = wallet_module
        .await_final_send_operation_state(operation_id)
        .await
        .map_err(|e| anyhow!("Error withdrawing funds onchain: {e}"))?;

    let fees = PegOutFees::from_amount(fee);

    match result {
        fedimint_walletv2_client::FinalSendOperationState::Success(txid) => {
            info!(target: LOG_GATEWAY, amount = %withdraw_amount, address = %address, "Sent funds via walletv2");
            Ok(WithdrawResponse { txid, fees })
        }
        fedimint_walletv2_client::FinalSendOperationState::Aborted => {
            Err(anyhow!("Withdrawal transaction was aborted"))
        }
        fedimint_walletv2_client::FinalSendOperationState::Failure => {
            Err(anyhow!("Withdrawal failed"))
        }
    }
}

impl Gateway {
    /// Opens the gateway database, ensures the root-entropy mnemonic exists,
    /// and builds the Fedimint client factory. This is the
    /// picomint-`GatewayClientFactory` analog: everything the gateway needs
    /// to build federation clients (and to derive the LDK node's entropy),
    /// assembled before the node is built.
    ///
    /// `gatewaydv2` has no setup UI and never imports a seed: it always boots
    /// with a freshly generated mnemonic, persisted once as the root entropy.
    /// The gateway's identity keypair and all per-federation client secrets
    /// derive from this single root entropy.
    pub async fn prepare(
        data_dir: &std::path::Path,
    ) -> anyhow::Result<(Database, GatewayClientBuilder, Mnemonic)> {
        let gateway_db = Database::new(
            fedimint_rocksdb::RocksDb::build(data_dir.join(DB_FILE))
                .open()
                .await?,
            ModuleDecoderRegistry::default(),
        );

        if Self::load_mnemonic(&gateway_db).await.is_none() {
            debug!(target: LOG_GATEWAY, "Generating mnemonic and writing root entropy to storage");
            let mnemonic = Bip39RootSecretStrategy::<12>::random(&mut OsRng);
            let mut dbtx = gateway_db.begin_transaction().await;
            dbtx.save_root_entropy(&mnemonic.to_entropy()).await;
            dbtx.commit_tx().await;
        }
        let mnemonic = Self::load_mnemonic(&gateway_db)
            .await
            .expect("root entropy was just ensured");

        // Gateway module will be attached when the federation clients are created
        // because the LN RPC will be injected with `GatewayClientGen`.
        let mut registry = ClientModuleInitRegistry::new();
        registry.attach(MintV2ClientInit);
        registry.attach(fedimint_walletv2_client::WalletClientInit);

        let client_builder = GatewayClientBuilder::new(data_dir.to_owned(), registry).await?;

        Ok((gateway_db, client_builder, mnemonic))
    }

    /// Builds the chain source for the LDK node from the configured RPC,
    /// reading bitcoind credentials embedded in the URL (`http://user:pass@host:port`).
    /// LDK uses bitcoind purely as a chain-data source via node-level RPCs, so
    /// -- unlike v1 gatewayd -- no watch-only wallet is created.
    pub fn chain_source(opts: &GatewayOpts) -> ChainSource {
        match (opts.bitcoind_url.as_ref(), opts.esplora_url.as_ref()) {
            // Use bitcoind by default if both are set
            (Some(url), _) => ChainSource::Bitcoind {
                username: url.username().to_string(),
                password: url.password().unwrap_or_default().to_string(),
                server_url: url.clone(),
            },
            (None, Some(url)) => ChainSource::Esplora {
                server_url: url.clone(),
            },
            _ => unreachable!("ArgGroup already enforced XOR relation"),
        }
    }

    /// Assembles the shared gateway state around an already-built LDK `node`.
    /// The plumbing fields (federation map, pay lock-pool, pending-channel map)
    /// are initialized here so callers only supply the meaningful parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        node: Arc<ldk_node::Node>,
        client_builder: GatewayClientBuilder,
        gateway_db: Database,
        listen: SocketAddr,
        network: Network,
        default_routing_fees: PaymentFee,
        default_transaction_fees: PaymentFee,
    ) -> Gateway {
        Self {
            federation_manager: Arc::new(RwLock::new(FederationManager::new())),
            node,
            outbound_lightning_payment_lock_pool: Arc::new(lockable::LockPool::new()),
            pending_channels: Arc::new(RwLock::new(BTreeMap::new())),
            client_builder,
            gateway_db,
            listen,
            network,
            default_routing_fees,
            default_transaction_fees,
        }
    }

    /// Drives the LDK node's event queue until the task is aborted (on process
    /// shutdown). Inbound payments become claimable here; channel lifecycle
    /// events (`ChannelPending` / `ChannelClosed`) are forwarded to a waiting
    /// [`Gateway::open_channel`] caller. Each event is acknowledged with
    /// `event_handled` before the next is pulled. Modeled on picomint's
    /// `process_ldk_events`.
    pub async fn process_ldk_events(self) {
        info!(target: LOG_GATEWAY, "Gateway is running");
        loop {
            let event = self.node.next_event_async().await;
            self.process_ldk_event(event).await;
            if let Err(err) = self.node.event_handled() {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "LDK could not mark event handled");
            }
        }
    }

    async fn process_ldk_event(&self, event: ldk_node::Event) {
        match event {
            ldk_node::Event::PaymentClaimable {
                payment_hash,
                claimable_amount_msat,
                ..
            } => {
                let payment_hash =
                    sha256::Hash::from_slice(&payment_hash.0).expect("payment hash is 32 bytes");
                self.handle_payment_claimable(payment_hash, claimable_amount_msat)
                    .await;
            }
            ldk_node::Event::PaymentSuccessful {
                payment_hash,
                payment_preimage: Some(preimage),
                ..
            } => {
                let payment_hash =
                    sha256::Hash::from_slice(&payment_hash.0).expect("payment hash is 32 bytes");
                self.handle_payment_successful(payment_hash, preimage.0)
                    .await;
            }
            ldk_node::Event::PaymentFailed {
                payment_hash: Some(payment_hash),
                ..
            } => {
                let payment_hash =
                    sha256::Hash::from_slice(&payment_hash.0).expect("payment hash is 32 bytes");
                self.handle_payment_failed(payment_hash).await;
            }
            ldk_node::Event::ChannelPending {
                channel_id,
                user_channel_id,
                funding_txo,
                ..
            } => {
                info!(target: LOG_GATEWAY, %channel_id, "LDK Channel is pending");
                if let Some(sender) = self
                    .pending_channels
                    .write()
                    .await
                    .remove(&UserChannelId(user_channel_id))
                {
                    let _ = sender.send(Ok(funding_txo));
                }
            }
            ldk_node::Event::ChannelClosed {
                channel_id,
                user_channel_id,
                reason,
                ..
            } => {
                info!(target: LOG_GATEWAY, %channel_id, "LDK Channel is closed");
                if let Some(sender) = self
                    .pending_channels
                    .write()
                    .await
                    .remove(&UserChannelId(user_channel_id))
                {
                    let reason = reason
                        .map_or_else(|| "Channel has been closed".to_string(), |r| r.to_string());
                    let _ = sender.send(Err(anyhow!(reason)));
                }
            }
            _ => {}
        }
    }

    /// Handles an inbound Lightning payment that the LDK node reports as
    /// claimable. If it matches a registered LNv2 incoming contract of the
    /// expected amount, submits the incoming-contract funding tx and spawns the
    /// federation-local Receive state machine (via [`start_receive`]);
    /// otherwise (or if funding fails, e.g. insufficient gateway liquidity)
    /// fails the HTLC so the sender is refunded promptly.
    ///
    /// The upstream HTLC is *not* claimed here — that is driven out-of-band by
    /// the per-client receive trailer once the Receive SM reaches success.
    /// `mark_ldk_event_processed` records that a real HTLC arrived, which the
    /// trailer uses to tell an external LN receive (claim the HTLC) from an
    /// internal direct swap (no HTLC to claim).
    ///
    /// [`start_receive`]: fedimint_gwv2_client::GatewayClientModuleV2::start_receive
    async fn handle_payment_claimable(&self, payment_hash: sha256::Hash, amount_msat: u64) {
        // The single-threaded event loop processes one event at a time, so this
        // read-then-act on the processed set is not racy.
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .is_ldk_event_processed(payment_hash.to_byte_array())
            .await
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_incoming_contract(operation_id)
            .await;

        let Some(registered) = registered else {
            warn!(
                target: LOG_GATEWAY,
                %payment_hash,
                "Claimable payment has no registered incoming contract; failing HTLC"
            );
            self.fail_htlc(payment_hash);
            return;
        };

        // The HTLC pays the invoice amount, which is the contract amount plus
        // the gateway's receive fee — so validate against the issued invoice,
        // not `contract.commitment.amount`.
        let LightningInvoice::Bolt11(invoice) = &registered.invoice;
        if invoice.amount_milli_satoshis() != Some(amount_msat) {
            warn!(
                target: LOG_GATEWAY,
                %payment_hash,
                "Claimable payment amount does not match the issued invoice; failing HTLC"
            );
            self.fail_htlc(payment_hash);
            return;
        }

        // The client-side operation is keyed by the contract (matching
        // `relay_direct_swap`, which the Send state machine uses for direct
        // swaps), so both receive paths land on the same operation.
        let client_operation_id = OperationId::from_encodable(&registered.contract);

        let start = async {
            self.select_client(registered.federation_id)
                .await?
                .into_value()
                .get_first_module::<GatewayClientModuleV2>()
                .expect("Must have client module")
                .start_receive(client_operation_id, registered.contract, amount_msat)
                .await
        };

        match start.await {
            Ok(..) => {
                let mut dbtx = self.gateway_db.begin_transaction().await;
                dbtx.mark_ldk_event_processed(payment_hash.to_byte_array())
                    .await;
                dbtx.commit_tx().await;
            }
            Err(err) => {
                // Funding the incoming contract failed (e.g. the gateway is not
                // pegged in / has insufficient liquidity). Fail the HTLC back so
                // the sender is refunded, matching the v1 relay-error path.
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to start incoming receive; failing HTLC");
                self.fail_htlc(payment_hash);
            }
        }
    }

    /// Handles a successful outbound LN payment: looks up the outgoing
    /// contract row and finalizes the send on the source federation with the
    /// preimage carried on the `PaymentSuccessful` event. Payments without a
    /// row (e.g. operator-initiated sends) are ignored.
    async fn handle_payment_successful(&self, payment_hash: sha256::Hash, preimage: [u8; 32]) {
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .is_ldk_event_processed(payment_hash.to_byte_array())
            .await
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        if let Some(row) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_outgoing_contract(operation_id)
            .await
        {
            self.finalize_send_for(
                row.federation_id,
                row.contract,
                row.outpoint,
                Some(preimage),
            )
            .await;
        }

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.mark_ldk_event_processed(payment_hash.to_byte_array())
            .await;
        dbtx.commit_tx().await;
    }

    /// Handles a failed outbound LN payment: looks up the outgoing contract
    /// row and forfeits the send on the source federation so the sender is
    /// refunded. Payments without a row are ignored.
    async fn handle_payment_failed(&self, payment_hash: sha256::Hash) {
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .is_ldk_event_processed(payment_hash.to_byte_array())
            .await
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        if let Some(row) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_outgoing_contract(operation_id)
            .await
        {
            self.finalize_send_for(row.federation_id, row.contract, row.outpoint, None)
                .await;
        }

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.mark_ldk_event_processed(payment_hash.to_byte_array())
            .await;
        dbtx.commit_tx().await;
    }

    /// Fails an inbound HTLC back to the sender (refund). Best-effort: logs on
    /// error since the node may retry event delivery.
    fn fail_htlc(&self, payment_hash: sha256::Hash) {
        if let Err(err) = self.fail_for_hash(payment_hash) {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to fail HTLC");
        }
    }

    /// Claims an inbound HTLC with `preimage` (settle). Best-effort; logs on
    /// error.
    fn claim_htlc(&self, payment_hash: sha256::Hash, preimage: [u8; 32]) {
        if let Err(err) = self.claim_for_hash(payment_hash, preimage) {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to claim HTLC");
        }
    }

    /// Returns the `ClientConfig` for each federation the gateway is connected
    /// to.
    pub async fn handle_get_federation_config(
        &self,
        federation_id_or: Option<FederationId>,
    ) -> AdminResult<GatewayFedConfig> {
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
            Err(anyhow!("No wallet module found"))
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
        .ok_or_else(|| anyhow!("Invalid ecash format: could not parse as ECash"))?;

        let client = self
            .federation_manager
            .read()
            .await
            .get_client_for_federation_id_prefix(federation_id_prefix)
            .ok_or_else(|| anyhow!("No federation available for prefix {federation_id_prefix}"))?;

        let mint = client
            .value()
            .get_first_module::<MintV2ClientModule>()
            .expect("MintV2 module is always attached to gateway clients");

        let ecash: fedimint_mintv2_client::ECash =
            base32::decode_prefixed(FEDIMINT_PREFIX, &payload.notes)
                .map_err(|e| anyhow!("Expected ECash for MintV2 federation: {e}"))?;
        let amount = ecash.amount();

        let operation_id = mint
            .receive(ecash, serde_json::Value::Null)
            .await
            .map_err(|e| anyhow!("{e}"))?;

        if payload.wait {
            let final_state = mint
                .await_final_receive_operation_state(operation_id)
                .await
                .map_err(|e| anyhow!("{e}"))?;
            match final_state {
                fedimint_mintv2_client::FinalReceiveOperationState::Success => {}
                fedimint_mintv2_client::FinalReceiveOperationState::Rejected => {
                    bail!("ECash receive was rejected");
                }
            }
        }

        Ok(ReceiveEcashResponse { amount })
    }

    /// Retrieves the client for a federation, building it on demand if it is
    /// not yet loaded. Clients are loaded lazily — nothing is built at boot;
    /// each federation's client is constructed the first time it is needed and
    /// cached thereafter (double-checked locking).
    ///
    /// Returns a concrete boxed future (rather than being an `async fn`) to
    /// break the async type cycle: building a client spawns its receive
    /// trailer, which itself lazily loads clients via `select_client`.
    pub fn select_client(
        &self,
        federation_id: FederationId,
    ) -> futures::future::BoxFuture<'_, AdminResult<Spanned<fedimint_client::ClientHandleArc>>>
    {
        Box::pin(self.select_client_inner(federation_id))
    }

    async fn select_client_inner(
        &self,
        federation_id: FederationId,
    ) -> AdminResult<Spanned<fedimint_client::ClientHandleArc>> {
        let not_connected = || {
            anyhow!(
                "No federation available for prefix {}",
                federation_id.to_prefix()
            )
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

        // Spawn this federation's receive trailer exactly once, when the client
        // is first built. It tails the client's event log and settles inbound
        // Lightning HTLCs as receives reach a terminal state.
        tokio::spawn(
            self.clone()
                .run_receive_trailer(federation_id, client.value().clone()),
        );

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

    /// Lists every joined federation by reading its `ClientConfig` from the
    /// database. Never builds a client (picomint-style, balance-free): each
    /// entry is just the federation id and name read straight from the config.
    async fn list_federation_infos(&self) -> Vec<cli::FederationInfo> {
        let configs = {
            let mut dbtx = self.gateway_db.begin_transaction_nc().await;
            dbtx.load_client_configs().await
        };
        configs
            .into_iter()
            .map(|(federation_id, config)| cli::FederationInfo {
                federation_id,
                federation_name: config.global.federation_name().map(str::to_string),
            })
            .collect()
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
                return Err(anyhow!(
                    "Federation {} is missing the required {kind} module",
                    config.calculate_federation_id()
                ));
            }
        }

        Ok(())
    }
}

/// Admin/operator request handlers, called by the REST API. (`gatewaydv2` has
/// no web UI, so these are plain inherent methods rather than an
/// `IAdminGateway` trait impl.)
impl Gateway {
    /// Returns a list of Lightning network channels from the Gateway's
    /// Lightning node.
    fn handle_list_channels_msg(&self) -> Vec<fedimint_gateway_common::ChannelInfo> {
        self.list_channels()
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
    async fn handle_leave_federation(&self, payload: LeaveFedPayload) -> AdminResult<()> {
        let federation_id = payload.federation_id;

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.load_client_config(federation_id)
            .await
            .ok_or_else(|| {
                anyhow!(
                    "No federation available for prefix {}",
                    federation_id.to_prefix()
                )
            })?;
        dbtx.save_disabled_federation(federation_id).await;
        dbtx.commit_tx().await;

        Ok(())
    }

    /// Handles a connection request to join a new federation. The gateway will
    /// download the federation's client configuration, construct a new
    /// client, registers, the gateway with the federation, and persists the
    /// necessary config to reconstruct the client when restarting the gateway.
    async fn handle_connect_federation(&self, invite: InviteCode) -> AdminResult<()> {
        // If the config is already persisted, connecting simply re-enabled the
        // federation; no need to re-download or build (the client loads lazily
        // on first use).
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_client_config(invite.federation_id())
            .await
            .is_some()
        {
            return Ok(());
        }

        // Fresh connection: download and persist the config WITHOUT building a
        // client. The client (and its first join) happens lazily on first use.
        let config = self
            .client_builder
            .download_config(&invite, Arc::new(self.clone()))
            .await?;

        Self::ensure_v2_modules(&config)?;

        let mut dbtx = self.gateway_db.begin_transaction().await;

        dbtx.save_client_config(&invite.federation_id(), &config)
            .await;

        dbtx.commit_tx().await;

        Ok(())
    }

    /// Handles an authenticated request for the gateway's mnemonic. This also
    /// returns a vector of federations that are not using the mnemonic
    /// backup strategy.
    async fn handle_mnemonic_msg(&self) -> AdminResult<cli::MnemonicResponse> {
        let mnemonic = Self::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");
        let words = mnemonic
            .words()
            .map(std::string::ToString::to_string)
            .collect::<Vec<_>>();
        Ok(cli::MnemonicResponse { mnemonic: words })
    }

    /// Instructs the Gateway's Lightning node to open a channel to a peer
    /// specified by `pubkey`.
    async fn handle_open_channel_msg(&self, payload: OpenChannelRequest) -> AdminResult<Txid> {
        info!(target: LOG_GATEWAY, pubkey = %payload.pubkey, host = %payload.host, amount = %payload.channel_size_sats, "Opening Lightning channel...");
        let funding_txid = self.open_channel(payload).await?;
        info!(target: LOG_GATEWAY, txid = %funding_txid, "Initiated channel open");
        Txid::from_str(&funding_txid)
            .map_err(|e| anyhow!("Received invalid channel funding txid string {e}"))
    }

    /// Instructs the Gateway's Lightning node to close all channels with a peer
    /// specified by `pubkey`.
    fn handle_close_channels_with_peer_msg(
        &self,
        payload: &CloseChannelsWithPeerRequest,
    ) -> CloseChannelsWithPeerResponse {
        info!(target: LOG_GATEWAY, close_channel_request = %payload, "Closing lightning channel...");
        let response = self.close_channels_with_peer(payload);
        info!(target: LOG_GATEWAY, close_channel_request = %payload, "Initiated channel closure");
        response
    }

    /// Send funds from the gateway's lightning node on-chain wallet.
    fn handle_send_onchain_msg(&self, payload: &SendOnchainRequest) -> AdminResult<Txid> {
        let response = self.send_onchain(payload.clone())?;
        let txid = Txid::from_str(&response)
            .map_err(|e| anyhow!("Failed to parse withdrawal TXID: {e}"))?;
        info!(onchain_request = %payload, txid = %txid, "Sent onchain transaction");
        Ok(txid)
    }

    /// Generates an onchain address to fund the gateway's lightning node.
    fn handle_get_ln_onchain_address_msg(&self) -> AdminResult<Address> {
        let response = self.get_ln_onchain_address()?;

        let address = Address::from_str(&response).map_err(|e| anyhow!("{e}"))?;

        address
            .require_network(self.network)
            .map_err(|e| anyhow!("{e}"))
    }

    /// Creates an invoice that is directly payable to the gateway's lightning
    /// node.
    fn handle_create_invoice_for_operator_msg(
        &self,
        payload: CreateInvoiceForOperatorPayload,
    ) -> AdminResult<Bolt11Invoice> {
        // No payment hash: this invoice is payable directly to the gateway.
        let description = Bolt11InvoiceDescription::Direct(payload.description.unwrap_or_default());
        self.create_invoice(
            None,
            payload.amount_msats,
            &description,
            payload.expiry_secs.unwrap_or(3600),
        )
        .map_err(|e| anyhow!("{e}"))
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

        let max_fee = BASE_FEE
            + payload
                .invoice
                .amount_milli_satoshis()
                .context("Invoice is missing amount")?
                .saturating_div(FEE_DENOMINATOR);

        let preimage = self
            .pay(&payload.invoice, MAX_DELAY, Amount::from_msats(max_fee))
            .await?;
        Ok(preimage)
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

        let mint_module = client
            .get_first_module::<MintV2ClientModule>()
            .expect("MintV2 module is always attached to gateway clients");

        let (_, ecash) = mint_module
            .send(payload.amount, serde_json::Value::Null, true)
            .await?;

        Ok(SpendEcashResponse {
            notes: base32::encode_prefixed(FEDIMINT_PREFIX, &ecash),
        })
    }

    /// Returns a Bitcoin TXID from a peg-out transaction for a specific
    /// connected federation.
    async fn handle_withdraw_msg(
        &self,
        federation_id: FederationId,
        address: Address<NetworkUnchecked>,
        amount: bitcoin::Amount,
    ) -> AdminResult<WithdrawResponse> {
        let address_network = get_network_for_address(&address);
        let gateway_network = self.network;
        let Ok(address) = address.require_network(gateway_network) else {
            return Err(anyhow!(
                "Gateway is running on network {gateway_network}, but provided withdraw address is for network {address_network}"
            ));
        };

        let client = self.select_client(federation_id).await?;

        let wallet_module = client
            .value()
            .get_first_module::<fedimint_walletv2_client::WalletClientModule>()?;

        withdraw_v2(&wallet_module, &address, amount).await
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
            lightning_public_key: self.public_key(),
            lightning_alias: Some(self.alias()),
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

    /// Orchestrates an outgoing LNv2 payment, modeled on picomint's
    /// `AppState::send`. Verifies the request, registers the contract in the
    /// daemon-global outgoing-contract table, logs the send-started event on
    /// the source federation, and kicks off either a direct-swap receive on
    /// the target federation or a fire-and-forget LN send via LDK. Returns
    /// once a terminal [`OutgoingPaymentSucceeded`] / [`OutgoingPaymentFailed`]
    /// event is observed in the source federation's event log: the terminal is
    /// driven out-of-band by the LDK `PaymentSuccessful`/`PaymentFailed`
    /// handlers (external sends) or the receive trailer (direct swaps).
    ///
    /// No [`SendStateMachine`] is spawned — the daemon owns the payment.
    ///
    /// [`SendStateMachine`]: fedimint_gwv2_client::GatewayClientModuleV2
    async fn send_payment_v2(
        &self,
        payload: SendPaymentPayload,
    ) -> Result<std::result::Result<[u8; 32], Signature>> {
        let f1_client = self
            .select_client(payload.federation_id)
            .await?
            .into_value();

        let f1_module = f1_client
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module");

        // --- Verify the request -------------------------------------------

        ensure!(
            payload.contract.claim_pk == f1_module.keypair.public_key(),
            "The outgoing contract is keyed to another gateway"
        );

        // This prevents DOS attacks where an attacker submits a different
        // invoice for the sender's contract.
        ensure!(
            payload.contract.verify_invoice_auth(
                payload.invoice.consensus_hash::<sha256::Hash>(),
                &payload.auth,
            ),
            "Invalid auth signature for the invoice data"
        );

        // The contract must be confirmed by the federation before we act on it
        // to prevent DOS attacks.
        let (contract_id, expiration) = f1_module
            .module_api
            .outgoing_contract_expiration(payload.outpoint)
            .await
            .map_err(|_| anyhow!("The gateway can not reach the federation"))?
            .ok_or(anyhow!("The outgoing contract has not yet been confirmed"))?;

        ensure!(
            contract_id == payload.contract.contract_id(),
            "Contract Id returned by the federation does not match contract in request"
        );

        let LightningInvoice::Bolt11(invoice) = payload.invoice.clone();

        let payment_hash = *invoice.payment_hash();

        let amount = invoice
            .amount_milli_satoshis()
            .ok_or(anyhow!("Invoice is missing amount"))?;

        ensure!(
            PaymentImage::Hash(payment_hash) == payload.contract.payment_image,
            "The invoices payment hash does not match the contracts payment hash"
        );

        let min_contract_amount = self
            .routing_info_v2(&payload.federation_id)
            .await?
            .ok_or(anyhow!("Routing Info not available"))?
            .send_fee_minimum
            .add_to(amount);

        let operation_id = OperationId::from_encodable(&payment_hash);

        // --- Register the outgoing contract; short-circuit on retry -------

        let row = OutgoingContractRow {
            federation_id: payload.federation_id,
            contract: payload.contract.clone(),
            outpoint: payload.outpoint,
            invoice: payload.invoice.clone(),
        };

        let mut dbtx = self.gateway_db.begin_transaction().await;

        if dbtx
            .save_outgoing_contract(operation_id, row)
            .await
            .is_some()
        {
            // A previous request already owns this payment; await its terminal.
            return self.await_send_terminal(&f1_client, operation_id).await;
        }

        dbtx.commit_tx().await;

        f1_module
            .log_send_started(
                operation_id,
                payload.contract.clone(),
                min_contract_amount,
                Amount::from_msats(amount),
                expiration.saturating_sub(EXPIRATION_DELTA_MINIMUM_V2),
            )
            .await;

        // --- Kick off the payment; failures cancel via finalize_send ------

        if let Err(err) = self.start_send(&payload, &invoice, min_contract_amount, expiration) {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to kick off outgoing payment; cancelling");
            if let Err(err) = f1_module
                .finalize_send(
                    operation_id,
                    payload.contract.clone(),
                    payload.outpoint,
                    None,
                )
                .await
            {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to cancel outgoing payment");
            }
        }

        // --- Await the terminal event on the source federation ------------

        self.await_send_terminal(&f1_client, operation_id).await
    }

    /// Kicks off the external LN payment or the direct-swap receive for a
    /// registered outgoing contract. Any error is turned into a cancellation
    /// (forfeit) by the caller.
    fn start_send(
        &self,
        payload: &SendPaymentPayload,
        invoice: &Bolt11Invoice,
        min_contract_amount: Amount,
        expiration: u64,
    ) -> Result<()> {
        ensure!(!invoice.is_expired(), "The invoice has already expired");

        let max_delay = expiration.saturating_sub(EXPIRATION_DELTA_MINIMUM_V2);

        ensure!(
            max_delay > 0,
            "The contract expiration is too close to forward the payment"
        );

        let max_fee = payload
            .contract
            .amount
            .checked_sub(min_contract_amount)
            .ok_or(anyhow!("The outgoing contract is underfunded"))?;

        if self.public_key() == invoice.get_payee_pub_key() {
            // Direct swap: the invoice was issued by this gateway, so a
            // registered incoming contract is the payment's target. Fund it;
            // the receive trailer finalizes the send once the receive settles.
            let gateway = self.clone();
            let contract = payload.contract.clone();
            let outpoint = payload.outpoint;
            let federation_id = payload.federation_id;
            let payment_hash = *invoice.payment_hash();
            let amount = invoice
                .amount_milli_satoshis()
                .expect("amount was checked by the caller");

            tokio::spawn(async move {
                if let Err(err) = gateway.start_direct_swap(payment_hash, amount).await {
                    warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to start direct swap; cancelling");
                    gateway
                        .finalize_send_for(federation_id, contract, outpoint, None)
                        .await;
                }
            });

            Ok(())
        } else {
            // External LN payment, fire-and-forget: the outcome arrives via
            // the LDK `PaymentSuccessful` / `PaymentFailed` events.
            self.send_bolt11_payment(invoice, max_delay, max_fee)
                .map_err(|e| anyhow!("{e}"))
        }
    }

    /// Funds the registered incoming contract that is the target of a direct
    /// swap (see [`Gateway::start_send`]).
    async fn start_direct_swap(&self, payment_hash: sha256::Hash, amount_msat: u64) -> Result<()> {
        let operation_id = OperationId::from_encodable(&payment_hash);

        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_incoming_contract(operation_id)
            .await
            .ok_or(anyhow!("No corresponding decryption contract available"))?;

        let LightningInvoice::Bolt11(invoice) = &registered.invoice;
        ensure!(
            invoice.amount_milli_satoshis() == Some(amount_msat),
            "Direct-swap amount mismatch"
        );

        let client_operation_id = OperationId::from_encodable(&registered.contract);

        self.select_client(registered.federation_id)
            .await?
            .into_value()
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .start_receive(client_operation_id, registered.contract, amount_msat)
            .await?;

        Ok(())
    }

    /// Tails the operation's event log on the source federation until the send
    /// reaches a terminal event, returning the preimage on success or the
    /// forfeit signature on cancellation. Replays history, so a completed
    /// operation returns immediately.
    async fn await_send_terminal(
        &self,
        client: &ClientHandleArc,
        operation_id: OperationId,
    ) -> Result<std::result::Result<[u8; 32], Signature>> {
        let mut stream = client.subscribe_operation_events(operation_id);

        while let Some(entry) = stream.next().await {
            if let Some(ev) = as_gw_event::<OutgoingPaymentSucceeded>(&entry) {
                return Ok(Ok(ev.preimage.expect("preimage is always recorded")));
            }

            if let Some(ev) = as_gw_event::<OutgoingPaymentFailed>(&entry) {
                warn!(target: LOG_GATEWAY, error = ?ev.error, "Outgoing lightning payment is cancelled");
                return Ok(Err(ev
                    .forfeit_signature
                    .expect("forfeit signature is always recorded")));
            }
        }

        Err(anyhow!(
            "Event stream ended before the send reached a terminal state"
        ))
    }

    /// Finalizes an outgoing contract on its source federation: claims it with
    /// `Some(preimage)` or forfeits it with `None`. Best-effort; logs on error.
    async fn finalize_send_for(
        &self,
        federation_id: FederationId,
        contract: fedimint_lnv2_common::contracts::OutgoingContract,
        outpoint: fedimint_core::OutPoint,
        preimage: Option<[u8; 32]>,
    ) {
        let PaymentImage::Hash(payment_hash) = contract.payment_image else {
            warn!(target: LOG_GATEWAY, "Outgoing contract has no payment hash");
            return;
        };

        let operation_id = OperationId::from_encodable(&payment_hash);

        let finalize = async {
            self.select_client(federation_id)
                .await?
                .into_value()
                .get_first_module::<GatewayClientModuleV2>()
                .expect("Must have client module")
                .finalize_send(operation_id, contract, outpoint, preimage)
                .await
        };

        if let Err(err) = finalize.await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to finalize outgoing payment");
        }
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
            bail!("The contract is invalid");
        }

        let payment_info = self
            .routing_info_v2(&payload.federation_id)
            .await?
            .with_context(|| format!("Federation {} does not exist", payload.federation_id))?;

        if payload.contract.commitment.refund_pk != payment_info.module_public_key {
            bail!("The incoming contract is keyed to another gateway");
        }

        let contract_amount = payment_info.receive_fee.subtract_from(payload.amount.msats);

        if contract_amount == Amount::ZERO {
            bail!("Zero amount incoming contracts are not supported");
        }

        if contract_amount != payload.contract.commitment.amount {
            bail!("The contract amount does not pay the correct amount of fees");
        }

        if payload.contract.commitment.expiration_or_fee <= duration_since_epoch().as_secs() {
            bail!("The contract has already expired");
        }

        let payment_hash = match payload.contract.commitment.payment_image {
            PaymentImage::Hash(payment_hash) => payment_hash,
            PaymentImage::Point(..) => {
                bail!("PaymentImage is not a payment hash");
            }
        };

        let invoice = self.create_invoice_via_lnrpc_v2(
            payment_hash,
            payload.amount,
            &payload.description,
            payload.expiry_secs,
        )?;

        let operation_id = OperationId::from_encodable(&payment_hash);

        let row = IncomingContractRow {
            federation_id: payload.federation_id,
            contract: payload.contract,
            invoice: LightningInvoice::Bolt11(invoice.clone()),
        };

        let mut dbtx = self.gateway_db.begin_transaction().await;

        if dbtx
            .save_incoming_contract(operation_id, row)
            .await
            .is_some()
        {
            bail!("PaymentHash is already registered");
        }

        dbtx.commit_tx_result()
            .await
            .map_err(|_| anyhow!("Payment hash is already registered"))?;

        Ok(invoice)
    }

    /// Retrieves a BOLT11 invoice from the connected Lightning node with a
    /// specific `payment_hash`.
    pub fn create_invoice_via_lnrpc_v2(
        &self,
        payment_hash: sha256::Hash,
        amount: Amount,
        description: &Bolt11InvoiceDescription,
        expiry_time: u32,
    ) -> std::result::Result<Bolt11Invoice, LightningRpcError> {
        self.create_invoice(Some(payment_hash), amount.msats, description, expiry_time)
    }

    pub async fn verify_bolt11_preimage_v2(
        &self,
        payment_hash: sha256::Hash,
        wait: bool,
    ) -> std::result::Result<VerifyResponse, String> {
        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_incoming_contract(OperationId::from_encodable(&payment_hash))
            .await
            .ok_or("Unknown payment hash".to_string())?;

        let client = self
            .select_client(registered.federation_id)
            .await
            .map_err(|_| "Not connected to federation".to_string())?
            .into_value();

        // Client-side operations are keyed by the contract (see
        // `handle_payment_claimable` / `relay_direct_swap`).
        let operation_id = OperationId::from_encodable(&registered.contract);

        // Fast path: scan the operation's event log for a terminal receive.
        if !wait {
            for entry in client.read_operation_events(operation_id).await {
                if let Some(ev) = as_gw_event::<IncomingPaymentSucceeded>(&entry) {
                    return Ok(VerifyResponse {
                        settled: true,
                        preimage: Some(ev.preimage.expect("preimage is always recorded")),
                    });
                }
            }

            return Ok(VerifyResponse {
                settled: false,
                preimage: None,
            });
        }

        // Slow path: tail the operation's event log until it reaches a terminal.
        let mut stream = client.subscribe_operation_events(operation_id);

        while let Some(entry) = stream.next().await {
            if let Some(ev) = as_gw_event::<IncomingPaymentSucceeded>(&entry) {
                return Ok(VerifyResponse {
                    settled: true,
                    preimage: Some(ev.preimage.expect("preimage is always recorded")),
                });
            }

            if as_gw_event::<IncomingPaymentFailed>(&entry).is_some() {
                return Err("Payment has failed".to_string());
            }
        }

        Err("Event stream ended before the receive reached a terminal state".to_string())
    }

    /// Retrieves the persisted `CreateInvoicePayload` from the database
    /// specified by the `payment_hash` and the `ClientHandleArc` specified
    /// by the payload's `federation_id`.
    pub async fn get_registered_incoming_contract_and_client_v2(
        &self,
        payment_image: PaymentImage,
        amount_msats: u64,
    ) -> Result<(IncomingContract, ClientHandleArc)> {
        let PaymentImage::Hash(payment_hash) = payment_image else {
            bail!("PaymentImage is not a payment hash");
        };

        let operation_id = OperationId::from_encodable(&payment_hash);

        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_incoming_contract(operation_id)
            .await
            .ok_or(anyhow!("No corresponding decryption contract available"))?;

        // The sender pays the invoice amount, which is the contract amount plus
        // the gateway's receive fee — so validate against the issued invoice,
        // not `contract.commitment.amount`.
        let LightningInvoice::Bolt11(invoice) = &registered.invoice;
        if invoice.amount_milli_satoshis() != Some(amount_msats) {
            bail!(
                "The available decryption contract's amount is not equal to the requested amount"
            );
        }

        let client = self
            .select_client(registered.federation_id)
            .await?
            .into_value();

        Ok((registered.contract, client))
    }

    /// Per-federation receive trailer, spawned once when a client is built (see
    /// [`Gateway::select_client`]). Tails this federation's client event log
    /// from a persisted [`TrailerCursor`] and drives the external side effect
    /// that finalizes a receive, mirroring picomint's daemon-wide trailer but
    /// scoped to one client (fedimint event logs are per-client).
    ///
    /// The federation-local `ReceiveStateMachine` reaches a terminal
    /// [`IncomingPaymentSucceeded`] / [`IncomingPaymentFailed`] on its own; the
    /// trailer then claims or fails the upstream Lightning HTLC. Dispatches are
    /// idempotent, so a crashed trailer just re-runs from the cursor.
    ///
    /// [`TrailerCursor`]: crate::db::TrailerCursorKey
    pub async fn run_receive_trailer(self, federation_id: FederationId, client: ClientHandleArc) {
        const CHUNK: u64 = 100;

        let mut cursor = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .load_trailer_cursor(federation_id)
            .await;

        let mut log_event_added = client.log_event_added_rx();

        info!(target: LOG_GATEWAY, %federation_id, "Receive trailer running");

        loop {
            let entries = client.get_event_log(cursor, CHUNK).await;

            for entry in &entries {
                self.dispatch_receive_event(entry).await;
                cursor = Some(entry.id().next());
            }

            if let Some(cursor) = cursor
                && !entries.is_empty()
            {
                let mut dbtx = self.gateway_db.begin_transaction().await;
                dbtx.save_trailer_cursor(federation_id, cursor).await;
                dbtx.commit_tx().await;
            }

            // Caught up: block until the client orders a new event into the log.
            if (entries.len() as u64) < CHUNK && log_event_added.changed().await.is_err() {
                break;
            }
        }
    }

    /// Dispatches one event-log entry, driving the external side effect that
    /// makes a terminal receive final from the outside world's point of view
    /// (mirroring picomint's trailer dispatch):
    ///
    /// - Direct swap (the daemon has an outgoing-contract row for this payment
    ///   hash): finalize the send on the source federation so the sender gets
    ///   the preimage (or forfeit signature).
    /// - External LN receive (no outgoing row): claim the upstream HTLC on the
    ///   LDK node with the revealed preimage, or fail it back so the LN sender
    ///   is refunded.
    async fn dispatch_receive_event(&self, entry: &fedimint_eventlog::PersistedLogEntry) {
        if let Some(ev) = as_gw_event::<IncomingPaymentSucceeded>(entry) {
            let PaymentImage::Hash(payment_hash) = ev.payment_image else {
                return;
            };

            let preimage = ev.preimage.expect("preimage is always recorded");

            match self.load_direct_swap_target(payment_hash).await {
                Some(row) => {
                    self.finalize_send_for(
                        row.federation_id,
                        row.contract,
                        row.outpoint,
                        Some(preimage),
                    )
                    .await;
                }
                None => self.claim_htlc(payment_hash, preimage),
            }
        } else if let Some(ev) = as_gw_event::<IncomingPaymentFailed>(entry) {
            let PaymentImage::Hash(payment_hash) = ev.payment_image else {
                return;
            };

            match self.load_direct_swap_target(payment_hash).await {
                Some(row) => {
                    self.finalize_send_for(row.federation_id, row.contract, row.outpoint, None)
                        .await;
                }
                None => self.fail_htlc(payment_hash),
            }
        }
    }

    /// Returns the outgoing-contract row this receive is the direct-swap
    /// target of, if any.
    async fn load_direct_swap_target(
        &self,
        payment_hash: sha256::Hash,
    ) -> Option<OutgoingContractRow> {
        self.gateway_db
            .begin_transaction_nc()
            .await
            .load_outgoing_contract(OperationId::from_encodable(&payment_hash))
            .await
    }
}

#[async_trait]
impl IGatewayClientV2 for Gateway {
    async fn complete_htlc(&self, htlc_response: InterceptPaymentResponse) {
        loop {
            match self.complete_htlc_once(htlc_response.clone()) {
                Ok(..) => return,
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
        if self.public_key() == invoice.get_payee_pub_key() {
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
        self.pay(&invoice, max_delay, max_fee)
            .await
            .map(|preimage| preimage.0)
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
