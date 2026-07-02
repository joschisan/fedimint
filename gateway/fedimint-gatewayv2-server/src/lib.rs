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

pub mod cli;
pub mod client;
pub mod db;
pub mod public;
pub mod trailer;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail, ensure};
use async_trait::async_trait;
use axum::response::{IntoResponse, Response};
use bitcoin::hashes::{Hash, sha256};
use bitcoin::{Network, OutPoint};
use fedimint_client::ClientHandleArc;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::{ModuleKind, OperationId};
use fedimint_core::db::{Database, IDatabaseTransactionOpsCoreTyped as _};
use fedimint_core::encoding::Encodable as _;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::task::sleep;
use fedimint_core::time::duration_since_epoch;
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow};
use fedimint_core::{Amount, crit};
use fedimint_gateway_common::{ReceiveEcashPayload, ReceiveEcashResponse};
use fedimint_gwv2_client::api::GatewayFederationApi as _;
use fedimint_gwv2_client::events::{
    IncomingPaymentFailed, IncomingPaymentSucceeded, OutgoingPaymentFailed,
    OutgoingPaymentSucceeded,
};
use fedimint_gwv2_client::{
    EXPIRATION_DELTA_MINIMUM_V2, FinalReceiveState, GatewayClientModuleV2, IGatewayClientV2,
};
use fedimint_lightning::{InterceptPaymentResponse, LightningRpcError, PaymentAction, Preimage};
use fedimint_lnurl::VerifyResponse;
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use fedimint_lnv2_common::gateway_api::{
    CreateBolt11InvoicePayload, PaymentFee, RoutingInfo, SendPaymentPayload,
};
use fedimint_lnv2_common::{Bolt11InvoiceDescription, LightningInvoice};
use fedimint_logging::LOG_GATEWAY;
use fedimint_mintv2_client::MintClientModule as MintV2ClientModule;
use futures::StreamExt as _;
use lightning::ln::channelmanager::PaymentId;
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning_invoice::{
    Bolt11Invoice, Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description,
};
use reqwest::StatusCode;
use tokio::sync::{RwLock, oneshot};
use tracing::{info, warn};

use crate::client::GatewayClientFactory;
use crate::db::{
    ClientConfigKey, ClientConfigKeyPrefix, DisabledFederationKey, IncomingContractKey,
    IncomingContractRow, OutgoingContractKey, OutgoingContractRow, ProcessedLdkEventKey,
};

/// Name of the gateway's database that is used for metadata and configuration
/// storage.
pub const DB_FILE: &str = "gatewayd.db";

/// Name of the folder that the gateway uses to store its node database when
/// running in LDK mode.
pub const LDK_NODE_DB_FOLDER: &str = "ldk_node";

/// Error type for the gateway's HTTP and admin-socket handlers. Wraps
/// `anyhow::Error` and responds with `500` plus the error message. The public
/// routes are the LNv2 protocol, whose clients only branch on success vs
/// failure, so there is no per-category status code or message redaction.
#[derive(Debug)]
pub struct GatewayError(anyhow::Error);

impl<E> From<E> for GatewayError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

// `Display` (not `Error`) so `#[instrument(err)]` can render it, without making
// `GatewayError: Into<anyhow::Error>`, which would clash with the blanket
// `From` impl above.
impl std::fmt::Display for GatewayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl IntoResponse for GatewayError {
    fn into_response(self) -> Response {
        crit!(target: LOG_GATEWAY, err = %self.0.fmt_compact_anyhow(), "Gateway request failed");
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}

/// Decodes an event-log entry into a specific gateway event, gating on the
/// entry's `kind`/`module` first. `PersistedLogEntry::to_event` only attempts a
/// JSON deserialization, so without this gate a sibling event with a
/// compatible shape could decode by accident.
pub(crate) fn as_gw_event<E: fedimint_eventlog::Event>(
    entry: &fedimint_eventlog::PersistedLogEntry,
) -> Option<E> {
    (entry.kind == E::KIND && entry.module_kind() == E::MODULE.as_ref())
        .then(|| entry.to_event::<E>())
        .flatten()
}

/// Newtype over [`ldk_node::UserChannelId`] so it can key the
/// [`AppState::pending_channels`] map.
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

/// The shared, cheaply-cloneable gateway state. Constructed as a struct
/// literal in `main` and handed to every long-running task (webserver, admin
/// CLI, LDK event loop, per-federation trailers); modeled on picomint's
/// `AppState`.
#[derive(Clone)]
pub struct AppState {
    /// Federation clients by id, lazily loaded on first use (see
    /// [`AppState::select_client`]).
    pub clients: Arc<RwLock<BTreeMap<FederationId, ClientHandleArc>>>,

    /// The gateway's LDK lightning node, built in `main` before any task is
    /// spawned.
    pub node: Arc<ldk_node::Node>,

    /// Factory that builds a Fedimint client for a federation, which handles
    /// the communication with that federation.
    pub client_factory: GatewayClientFactory,

    /// Database for gateway metadata.
    pub gateway_db: Database,

    /// Path to the folder containing the gateway's config and data files. The
    /// admin CLI socket lives here.
    pub data_dir: PathBuf,

    /// The socket the gateway's public API webserver listens on.
    pub api_addr: SocketAddr,

    /// The Bitcoin network that the Lightning network is configured to.
    pub network: Network,

    /// The default routing fees for new federations.
    pub default_routing_fees: PaymentFee,

    /// The default transaction fees for new federations.
    pub default_transaction_fees: PaymentFee,

    /// Serializes concurrent [`AppState::pay`] calls for the same invoice so
    /// the (non-idempotent) LDK send runs at most once per payment hash.
    pub outbound_lightning_payment_lock_pool: Arc<lockable::LockPool<PaymentId>>,

    /// Channels currently being opened, keyed by `UserChannelId`. The LDK event
    /// loop uses the sender to hand the funding outpoint back to the waiting
    /// [`crate::cli`] open-channel caller.
    pub pending_channels:
        Arc<RwLock<BTreeMap<UserChannelId, oneshot::Sender<anyhow::Result<OutPoint>>>>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("client_factory", &self.client_factory)
            .field("gateway_db", &self.gateway_db)
            .field("api_addr", &self.api_addr)
            .finish_non_exhaustive()
    }
}

impl AppState {
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
    ) -> futures::future::BoxFuture<'_, anyhow::Result<ClientHandleArc>> {
        Box::pin(self.select_client_inner(federation_id))
    }

    async fn select_client_inner(
        &self,
        federation_id: FederationId,
    ) -> anyhow::Result<ClientHandleArc> {
        let not_connected = || {
            anyhow!(
                "No federation available for prefix {}",
                federation_id.to_prefix()
            )
        };

        // Fast path: the client is already loaded.
        if let Some(client) = self.clients.read().await.get(&federation_id).cloned() {
            return Ok(client);
        }

        // Slow path: build the client under the write lock.
        let mut clients = self.clients.write().await;

        // Re-check in case another task built it while we waited for the lock.
        if let Some(client) = clients.get(&federation_id).cloned() {
            return Ok(client);
        }

        // Only federations whose config is persisted can be built. The stored
        // `ClientConfig` is what lets the client be (lazily) joined without an
        // invite code.
        let Some(config) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&ClientConfigKey(federation_id))
            .await
        else {
            return Err(not_connected());
        };

        let mnemonic = client::load_mnemonic(&self.gateway_db)
            .await
            .expect("mnemonic should be set");

        let client = Box::pin(self.client_factory.open(
            federation_id,
            config,
            Arc::new(self.clone()),
            &mnemonic,
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

        clients.insert(federation_id, client.clone());

        // Spawn this federation's receive trailer exactly once, when the client
        // is first built. It tails the client's event log and settles inbound
        // Lightning HTLCs as receives reach a terminal state.
        tokio::spawn(trailer::run(self.clone(), federation_id, client.clone()));

        Ok(client)
    }

    /// Lists every joined federation by reading its `ClientConfig` from the
    /// database. Never builds a client (picomint-style, balance-free): each
    /// entry is just the federation id and name read straight from the config.
    pub async fn federation_list(&self) -> Vec<fedimint_gatewayv2_cli_core::FederationInfo> {
        self.gateway_db
            .begin_transaction_nc()
            .await
            .find_by_prefix(&ClientConfigKeyPrefix)
            .await
            .map(
                |(key, config)| fedimint_gatewayv2_cli_core::FederationInfo {
                    federation_id: key.0,
                    federation_name: config.global.federation_name().map(str::to_string),
                },
            )
            .collect()
            .await
    }

    /// Ensures the federation exposes the three v2 modules the gateway needs.
    ///
    /// This checks module *kinds* only, which works on the raw, undecoded
    /// config returned by `preview`, so it can run at connect time without
    /// building a client. The network is intentionally not validated here; a
    /// mismatch surfaces later when the client is built and operates.
    fn ensure_v2_modules(config: &ClientConfig) -> anyhow::Result<()> {
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

    /// Handles a connection request to join a new federation. The gateway will
    /// download the federation's client configuration and persist it so the
    /// client can be reconstructed (lazily, on first use) when restarting the
    /// gateway.
    pub async fn connect_federation(&self, invite: InviteCode) -> anyhow::Result<()> {
        // If the config is already persisted, connecting simply re-enables the
        // federation; no need to re-download or build (the client loads lazily
        // on first use).
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&ClientConfigKey(invite.federation_id()))
            .await
            .is_some()
        {
            return Ok(());
        }

        // Fresh connection: download and persist the config WITHOUT building a
        // client. The client (and its first join) happens lazily on first use.
        let config = self
            .client_factory
            .download_config(&invite, Arc::new(self.clone()))
            .await?;

        Self::ensure_v2_modules(&config)?;

        let mut dbtx = self.gateway_db.begin_transaction().await;

        dbtx.insert_entry(&ClientConfigKey(invite.federation_id()), &config)
            .await;

        dbtx.commit_tx().await;

        Ok(())
    }

    /// Handles a request to receive ecash into the gateway. Only a federation
    /// whose client is already loaded can be targeted, since the ecash bundle
    /// carries just a federation id prefix.
    pub async fn receive_ecash(
        &self,
        payload: ReceiveEcashPayload,
    ) -> anyhow::Result<ReceiveEcashResponse> {
        let federation_id_prefix = base32::decode_prefixed::<fedimint_mintv2_client::ECash>(
            FEDIMINT_PREFIX,
            &payload.notes,
        )
        .ok()
        .and_then(|e| e.mint())
        .map(|id| id.to_prefix())
        .ok_or_else(|| anyhow!("Invalid ecash format: could not parse as ECash"))?;

        let client = self
            .clients
            .read()
            .await
            .iter()
            .find_map(|(federation_id, client)| {
                (federation_id.to_prefix() == federation_id_prefix).then(|| client.clone())
            })
            .ok_or_else(|| anyhow!("No federation available for prefix {federation_id_prefix}"))?;

        let mint = client
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
}

// Lightning Gateway implementation
impl AppState {
    /// Returns payment information that LNv2 clients can use to instruct this
    /// Gateway to pay an invoice or receive a payment. Returns `None` if the
    /// federation is disabled or not joined; the client is lazily built to read
    /// its module public key.
    pub async fn gateway_info(
        &self,
        federation_id: &FederationId,
    ) -> anyhow::Result<Option<RoutingInfo>> {
        // Disabled federations advertise no routing info.
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&DisabledFederationKey(*federation_id))
            .await
            .is_some()
        {
            return Ok(None);
        }

        // Lazily load the client to read its module public key. A federation
        // that isn't joined (or fails to load) yields `None`.
        let Ok(client) = self.select_client(*federation_id).await else {
            return Ok(None);
        };
        let module_public_key = client
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .keypair
            .public_key();

        let lightning_fee = self.default_routing_fees;
        let transaction_fee = self.default_transaction_fees;

        Ok(Some(RoutingInfo {
            lightning_public_key: self.node.node_id(),
            lightning_alias: Some(self.node.node_alias().map_or_else(
                || format!("LDK Fedimint Gateway Node {}", self.node.node_id()),
                |alias| alias.to_string(),
            )),
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
    pub async fn send(
        &self,
        payload: SendPaymentPayload,
    ) -> anyhow::Result<std::result::Result<[u8; 32], Signature>> {
        let f1_client = self.select_client(payload.federation_id).await?;

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
            .gateway_info(&payload.federation_id)
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
            .insert_entry(&OutgoingContractKey(operation_id), &row)
            .await
            .is_some()
        {
            // A previous request already owns this payment; await its terminal.
            return Self::subscribe_send(&f1_client, operation_id).await;
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

        Self::subscribe_send(&f1_client, operation_id).await
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
    ) -> anyhow::Result<()> {
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

        if self.node.node_id() == invoice.get_payee_pub_key() {
            // Direct swap: the invoice was issued by this gateway, so a
            // registered incoming contract is the payment's target. Fund it;
            // the receive trailer finalizes the send once the receive settles.
            let state = self.clone();
            let contract = payload.contract.clone();
            let outpoint = payload.outpoint;
            let federation_id = payload.federation_id;
            let payment_hash = *invoice.payment_hash();
            let amount = invoice
                .amount_milli_satoshis()
                .expect("amount was checked by the caller");

            tokio::spawn(async move {
                if let Err(err) = state.start_direct_swap(payment_hash, amount).await {
                    warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to start direct swap; cancelling");
                    state
                        .finalize_send_for(federation_id, contract, outpoint, None)
                        .await;
                }
            });

            Ok(())
        } else {
            // External LN payment, fire-and-forget: the outcome arrives via
            // the LDK `PaymentSuccessful` / `PaymentFailed` events.
            self.node
                .bolt11_payment()
                .send(
                    invoice,
                    Some(ldk_node::payment::SendingParameters {
                        max_total_routing_fee_msat: Some(Some(max_fee.msats)),
                        max_total_cltv_expiry_delta: Some(max_delay as u32),
                        max_path_count: None,
                        max_channel_saturation_power_of_half: None,
                    }),
                )
                .map(|_| ())
                .map_err(|e| anyhow!("LDK payment failed to initialize: {e:?}"))
        }
    }

    /// Funds the registered incoming contract that is the target of a direct
    /// swap (see [`AppState::start_send`]).
    async fn start_direct_swap(
        &self,
        payment_hash: sha256::Hash,
        amount_msat: u64,
    ) -> anyhow::Result<()> {
        let operation_id = OperationId::from_encodable(&payment_hash);

        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&IncomingContractKey(operation_id))
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
    async fn subscribe_send(
        client: &ClientHandleArc,
        operation_id: OperationId,
    ) -> anyhow::Result<std::result::Result<[u8; 32], Signature>> {
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
    pub(crate) async fn finalize_send_for(
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
                .get_first_module::<GatewayClientModuleV2>()
                .expect("Must have client module")
                .finalize_send(operation_id, contract, outpoint, preimage)
                .await
        };

        if let Err(err) = finalize.await {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact_anyhow(), %payment_hash, "Failed to finalize outgoing payment");
        }
    }

    /// Creates a BOLT11 invoice for an incoming LNv2 payment by fetching it
    /// from the lightning node, then registers the incoming contract and the
    /// invoice under the payment hash so that inbound lightning payments can be
    /// matched as a receive attempt to a specific federation.
    pub async fn receive(
        &self,
        payload: CreateBolt11InvoicePayload,
    ) -> anyhow::Result<Bolt11Invoice> {
        if !payload.contract.verify() {
            bail!("The contract is invalid");
        }

        let payment_info = self
            .gateway_info(&payload.federation_id)
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

        let description = match &payload.description {
            Bolt11InvoiceDescription::Direct(description) => LdkBolt11InvoiceDescription::Direct(
                Description::new(description.clone())
                    .map_err(|_| anyhow!("Invalid invoice description"))?,
            ),
            Bolt11InvoiceDescription::Hash(hash) => {
                LdkBolt11InvoiceDescription::Hash(lightning_invoice::Sha256(*hash))
            }
        };

        let invoice = self
            .node
            .bolt11_payment()
            .receive_for_hash(
                payload.amount.msats,
                &description,
                payload.expiry_secs,
                PaymentHash(*payment_hash.as_byte_array()),
            )
            .map_err(|e| anyhow!("Failed to create LDK invoice: {e}"))?;

        let invoice = Bolt11Invoice::from_str(&invoice.to_string()).map_err(|e| anyhow!("{e}"))?;

        let operation_id = OperationId::from_encodable(&payment_hash);

        let row = IncomingContractRow {
            federation_id: payload.federation_id,
            contract: payload.contract,
            invoice: LightningInvoice::Bolt11(invoice.clone()),
        };

        let mut dbtx = self.gateway_db.begin_transaction().await;

        if dbtx
            .insert_entry(&IncomingContractKey(operation_id), &row)
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

    /// Checks whether a receive has settled, returning the preimage once it
    /// has. With `wait`, tails the operation's event log until it reaches a
    /// terminal state.
    pub async fn verify(
        &self,
        payment_hash: sha256::Hash,
        wait: bool,
    ) -> std::result::Result<VerifyResponse, String> {
        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&IncomingContractKey(OperationId::from_encodable(
                &payment_hash,
            )))
            .await
            .ok_or("Unknown payment hash".to_string())?;

        let client = self
            .select_client(registered.federation_id)
            .await
            .map_err(|_| "Not connected to federation".to_string())?;

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

    /// Retrieves the registered incoming contract for a payment image and the
    /// client for its federation, validating the requested amount against the
    /// issued invoice.
    async fn registered_incoming_contract(
        &self,
        payment_image: PaymentImage,
        amount_msats: u64,
    ) -> anyhow::Result<(IncomingContract, ClientHandleArc)> {
        let PaymentImage::Hash(payment_hash) = payment_image else {
            bail!("PaymentImage is not a payment hash");
        };

        let operation_id = OperationId::from_encodable(&payment_hash);

        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&IncomingContractKey(operation_id))
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

        let client = self.select_client(registered.federation_id).await?;

        Ok((registered.contract, client))
    }
}

// LDK event loop
impl AppState {
    /// Drives the LDK node's event queue until the task is aborted (on process
    /// shutdown). Inbound payments become claimable here; outbound payment
    /// outcomes finalize their outgoing contracts; channel lifecycle events
    /// (`ChannelPending` / `ChannelClosed`) are forwarded to a waiting
    /// [`crate::cli`] open-channel caller. Each event is acknowledged with
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
    /// The [`ProcessedLdkEventKey`] insert records that a real HTLC arrived,
    /// which the trailer uses to tell an external LN receive (claim the HTLC)
    /// from an internal direct swap (no HTLC to claim).
    ///
    /// [`start_receive`]: fedimint_gwv2_client::GatewayClientModuleV2::start_receive
    async fn handle_payment_claimable(&self, payment_hash: sha256::Hash, amount_msat: u64) {
        // The single-threaded event loop processes one event at a time, so this
        // read-then-act on the processed set is not racy.
        if self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&ProcessedLdkEventKey(payment_hash.to_byte_array()))
            .await
            .is_some()
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&IncomingContractKey(operation_id))
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
                .get_first_module::<GatewayClientModuleV2>()
                .expect("Must have client module")
                .start_receive(client_operation_id, registered.contract, amount_msat)
                .await
        };

        match start.await {
            Ok(..) => {
                let mut dbtx = self.gateway_db.begin_transaction().await;
                dbtx.insert_entry(&ProcessedLdkEventKey(payment_hash.to_byte_array()), &())
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
            .get_value(&ProcessedLdkEventKey(payment_hash.to_byte_array()))
            .await
            .is_some()
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        if let Some(row) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&OutgoingContractKey(operation_id))
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
        dbtx.insert_entry(&ProcessedLdkEventKey(payment_hash.to_byte_array()), &())
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
            .get_value(&ProcessedLdkEventKey(payment_hash.to_byte_array()))
            .await
            .is_some()
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        if let Some(row) = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&OutgoingContractKey(operation_id))
            .await
        {
            self.finalize_send_for(row.federation_id, row.contract, row.outpoint, None)
                .await;
        }

        let mut dbtx = self.gateway_db.begin_transaction().await;
        dbtx.insert_entry(&ProcessedLdkEventKey(payment_hash.to_byte_array()), &())
            .await;
        dbtx.commit_tx().await;
    }
}

// Lightning node access shared by the LNv2 protocol, the trailer, and the
// admin CLI. (Operator-only node management lives directly in [`crate::cli`],
// picomint-style.)
impl AppState {
    /// Claims (settles) a claimable inbound HTLC on the lightning node with the
    /// given `preimage`. Called by the receive trailer once the federation-side
    /// receive succeeds.
    pub(crate) fn claim_for_hash(
        &self,
        payment_hash: sha256::Hash,
        preimage: [u8; 32],
    ) -> std::result::Result<(), LightningRpcError> {
        let ph = PaymentHash(*payment_hash.as_byte_array());

        // TODO: Get the actual amount from the LDK node. This value is only used
        // by `ldk-node` to ensure that the amount claimed isn't less than the
        // amount expected, which we already verified when the payment arrived.
        let claimable_amount_msat = 999_999_999_999_999;

        self.node
            .bolt11_payment()
            .claim_for_hash(ph, claimable_amount_msat, PaymentPreimage(preimage))
            .map_err(|_| LightningRpcError::FailedToCompleteHtlc {
                failure_reason: format!("Failed to claim LDK payment with hash {payment_hash}"),
            })
    }

    /// Fails a claimable inbound HTLC back to the sender (refund).
    pub(crate) fn fail_for_hash(
        &self,
        payment_hash: sha256::Hash,
    ) -> std::result::Result<(), LightningRpcError> {
        let ph = PaymentHash(*payment_hash.as_byte_array());

        self.node.bolt11_payment().fail_for_hash(ph).map_err(|_| {
            LightningRpcError::FailedToCompleteHtlc {
                failure_reason: format!("Failed to fail LDK payment with hash {payment_hash}"),
            }
        })
    }

    /// Claims an inbound HTLC with `preimage` (settle). Best-effort; logs on
    /// error since the node may retry event delivery.
    pub(crate) fn claim_htlc(&self, payment_hash: sha256::Hash, preimage: [u8; 32]) {
        if let Err(err) = self.claim_for_hash(payment_hash, preimage) {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to claim HTLC");
        }
    }

    /// Fails an inbound HTLC back to the sender (refund). Best-effort; logs on
    /// error.
    pub(crate) fn fail_htlc(&self, payment_hash: sha256::Hash) {
        if let Err(err) = self.fail_for_hash(payment_hash) {
            warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "Failed to fail HTLC");
        }
    }

    /// Attempts to pay an invoice using the lightning node, waiting for the
    /// payment to complete and returning the preimage.
    ///
    /// This is idempotent for a given invoice: if a payment is already in
    /// flight it waits for that one to complete instead of starting another.
    pub async fn pay(
        &self,
        invoice: &Bolt11Invoice,
        max_delay: u64,
        max_fee: Amount,
    ) -> std::result::Result<Preimage, LightningRpcError> {
        let payment_id = PaymentId(*invoice.payment_hash().as_byte_array());

        // Lock by the payment hash to prevent multiple simultaneous calls with the same
        // invoice from executing. This prevents `ldk-node::Bolt11Payment::send()` from
        // being called multiple times with the same invoice. This is important because
        // `ldk-node::Bolt11Payment::send()` is not idempotent, but this function must
        // be idempotent.
        let _payment_lock_guard = self
            .outbound_lightning_payment_lock_pool
            .async_lock(payment_id)
            .await;

        // If a payment is not known to the node we can initiate it, and if it is known
        // we can skip calling `ldk-node::Bolt11Payment::send()` and wait for the
        // payment to complete. The lock guard above guarantees that this block is only
        // executed once at a time for a given payment hash, ensuring that there is no
        // race condition between checking if a payment is known and initiating a new
        // payment if it isn't.
        if self.node.payment(&payment_id).is_none() {
            assert_eq!(
                self.node
                    .bolt11_payment()
                    .send(
                        invoice,
                        Some(ldk_node::payment::SendingParameters {
                            max_total_routing_fee_msat: Some(Some(max_fee.msats)),
                            max_total_cltv_expiry_delta: Some(max_delay as u32),
                            max_path_count: None,
                            max_channel_saturation_power_of_half: None,
                        }),
                    )
                    // TODO: Investigate whether all error types returned by `Bolt11Payment::send()`
                    // result in idempotency.
                    .map_err(|e| LightningRpcError::FailedPayment {
                        failure_reason: format!("LDK payment failed to initialize: {e:?}"),
                    })?,
                payment_id
            );
        }

        // TODO: Find a way to avoid looping/polling to know when a payment is
        // completed. `ldk-node` provides `PaymentSuccessful` and `PaymentFailed`
        // events, but interacting with the node event queue here isn't
        // straightforward.
        loop {
            if let Some(payment_details) = self.node.payment(&payment_id) {
                match payment_details.status {
                    ldk_node::payment::PaymentStatus::Pending => {}
                    ldk_node::payment::PaymentStatus::Succeeded => {
                        if let ldk_node::payment::PaymentKind::Bolt11 {
                            preimage: Some(preimage),
                            ..
                        } = payment_details.kind
                        {
                            return Ok(Preimage(preimage.0));
                        }
                    }
                    ldk_node::payment::PaymentStatus::Failed => {
                        return Err(LightningRpcError::FailedPayment {
                            failure_reason: "LDK payment failed".to_string(),
                        });
                    }
                }
            }
            fedimint_core::runtime::sleep(Duration::from_millis(100)).await;
        }
    }

    /// Settles or fails a claimable inbound payment per the [`PaymentAction`].
    /// Retained only for the (dead-for-v2) `IGatewayClientV2::complete_htlc`
    /// trait method — the live receive path uses [`Self::claim_for_hash`] /
    /// [`Self::fail_for_hash`] directly.
    fn complete_htlc_once(
        &self,
        htlc: InterceptPaymentResponse,
    ) -> std::result::Result<(), LightningRpcError> {
        if let PaymentAction::Settle(preimage) = htlc.action {
            self.claim_for_hash(htlc.payment_hash, preimage.0)
        } else {
            warn!(target: LOG_GATEWAY, payment_hash = %htlc.payment_hash, "Unwinding payment because the action was not `Settle`");
            self.fail_for_hash(htlc.payment_hash)
        }
    }
}

#[async_trait]
impl IGatewayClientV2 for AppState {
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
        if self.node.node_id() == invoice.get_payee_pub_key() {
            let (contract, client) = self
                .registered_incoming_contract(
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
            .gateway_info(federation_id)
            .await?
            .ok_or(anyhow!("Routing Info not available"))?
            .send_fee_minimum
            .add_to(amount))
    }

    /// `gatewaydv2` does not support LNv1, so no invoice is ever treated as an
    /// LNv1 invoice and the LNv2 -> LNv1 swap path below is never taken.
    async fn is_lnv1_invoice(
        &self,
        _invoice: &Bolt11Invoice,
    ) -> Option<fedimint_core::util::Spanned<ClientHandleArc>> {
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
