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

pub mod analytics;
pub mod cli;
pub mod client;
pub mod db;
pub mod probe;
pub mod public;
pub mod trailer;

use std::collections::BTreeMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail, ensure};
use axum::response::{IntoResponse, Response};
use bitcoin::Network;
use bitcoin::hashes::{Hash, sha256};
use fedimint_client::ClientHandleArc;
use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::{ModuleKind, OperationId};
use fedimint_core::db::{Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped as _};
use fedimint_core::encoding::Encodable as _;
use fedimint_core::invite_code::InviteCode;
use fedimint_core::secp256k1::schnorr::Signature;
use fedimint_core::time::{duration_since_epoch, now};
use fedimint_core::util::{FmtCompact, FmtCompactAnyhow};
use fedimint_core::{Amount, crit};
use fedimint_gwv2_client::api::GatewayFederationApi as _;
use fedimint_gwv2_client::events::{
    IncomingPaymentFailed, IncomingPaymentSucceeded, OutgoingPaymentFailed, OutgoingPaymentStarted,
    OutgoingPaymentSucceeded,
};
use fedimint_gwv2_client::{Cancelled, EXPIRATION_DELTA_MINIMUM_V2, GatewayClientModuleV2};
use fedimint_lnurl::VerifyResponse;
use fedimint_lnv2_common::contracts::PaymentImage;
use fedimint_lnv2_common::gateway_api::{
    CreateBolt11InvoicePayload, PaymentFee, RoutingInfo, SendPaymentPayload,
};
use fedimint_lnv2_common::{Bolt11InvoiceDescription, LightningInvoice};
use fedimint_logging::LOG_GATEWAY;
use futures::StreamExt as _;
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning_invoice::{
    Bolt11Invoice, Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description,
};
use reqwest::StatusCode;
use tokio::sync::RwLock;
use tracing::{info, warn};

use crate::client::GatewayClientFactory;
use crate::db::{
    ClientConfigKey, ClientConfigKeyPrefix, DisabledFederationKey, IncomingContractKey,
    IncomingContractRow, OutgoingContractKey, OutgoingContractRow, ProcessedLdkEventKey,
    client_db_prefix,
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

    /// The gateway's fee cut on outgoing payments.
    pub send_fee: PaymentFee,

    /// The gateway's fee cut on incoming payments.
    pub receive_fee: PaymentFee,

    /// The Lightning routing fee budget, enforced exactly as LDK's
    /// `max_total_routing_fee_msat` cap on external outgoing payments.
    pub routing_fee: PaymentFee,

    /// SQLite mirror of the gwv2 payment events, wiped and rebuilt from the
    /// client event logs on every startup (see [`analytics`]).
    pub analytics: analytics::Analytics,
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

        let client = Box::pin(self.client_factory.open(federation_id, config, &mnemonic))
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

        // Spawn this federation's trailers exactly once, when the client is
        // first built. The receive trailer tails the client's event log and
        // settles inbound Lightning HTLCs as receives reach a terminal state;
        // the analytics trailer mirrors the payment events into SQLite.
        tokio::spawn(trailer::run(self.clone(), federation_id, client.clone()));
        tokio::spawn(analytics::trailer(
            self.analytics.clone(),
            federation_id,
            client.clone(),
        ));

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
        let config = self.client_factory.download_config(&invite).await?;

        Self::ensure_v2_modules(&config)?;

        let mut dbtx = self.gateway_db.begin_transaction().await;

        dbtx.insert_entry(&ClientConfigKey(invite.federation_id()), &config)
            .await;

        dbtx.commit_tx().await;

        Ok(())
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

        Ok(Some(RoutingInfo {
            lightning_public_key: self.node.node_id(),
            lightning_alias: Some(self.node.node_alias().map_or_else(
                || format!("LDK Fedimint Gateway Node {}", self.node.node_id()),
                |alias| alias.to_string(),
            )),
            module_public_key,
            send_fee_default: self.routing_fee + self.send_fee,
            // The base fee ensures that the gateway does not loose sats sending the payment due
            // to fees paid on the transaction claiming the outgoing contract or
            // subsequent transactions spending the newly issued ecash
            send_fee_minimum: self.send_fee,
            expiration_delta_default: 1440,
            expiration_delta_minimum: EXPIRATION_DELTA_MINIMUM_V2,
            // The base fee ensures that the gateway does not loose sats receiving the payment
            // due to fees paid on the transaction funding the incoming contract
            receive_fee: self.receive_fee,
        }))
    }

    /// Orchestrates an outgoing LNv2 payment, modeled on picomint's
    /// `AppState::send`. Verifies the request, then in one write transaction
    /// registers the contract in the daemon-global outgoing-contract table,
    /// logs the send-started event on the source federation, and kicks off
    /// either a direct-swap receive on the target federation or a
    /// fire-and-forget LN send via LDK. Returns once a terminal
    /// [`OutgoingPaymentSucceeded`] / [`OutgoingPaymentFailed`] event is
    /// observed in the source federation's event log: the terminal is driven
    /// out-of-band by the LDK `PaymentSuccessful`/`PaymentFailed` handlers
    /// (external sends) or the receive trailer (direct swaps).
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

        let amount = invoice
            .amount_milli_satoshis()
            .ok_or(anyhow!("Invoice is missing amount"))?;

        ensure!(
            PaymentImage::Hash(*invoice.payment_hash()) == payload.contract.payment_image,
            "The invoices payment hash does not match the contracts payment hash"
        );

        ensure!(!invoice.is_expired(), "The invoice has already expired");

        let min_contract_amount = self
            .gateway_info(&payload.federation_id)
            .await?
            .ok_or(anyhow!("Routing Info not available"))?
            .send_fee_minimum
            .add_to(amount);

        let max_fee = payload
            .contract
            .amount
            .checked_sub(min_contract_amount)
            .ok_or(anyhow!("The outgoing contract is underfunded"))?;

        let max_delay = expiration.saturating_sub(EXPIRATION_DELTA_MINIMUM_V2);

        ensure!(
            max_delay > 0,
            "The contract expiration is too close to forward the payment"
        );

        let operation_id = OperationId::from_encodable(invoice.payment_hash());

        // --- Insert the outgoing-contract row + log the send-started ------
        // --- event on F1 (one tx); short-circuit on retry ------------------

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
            // A previous request already owns this payment; drop the
            // re-insert and await its terminal.
            dbtx.ignore_uncommitted();

            return Self::subscribe_send(&f1_client, operation_id).await;
        }

        {
            let f1_client_dbtx = dbtx.to_ref_with_prefix(client_db_prefix(&payload.federation_id));

            let mut f1_module_dbtx = f1_client_dbtx
                .with_prefix_module_id(f1_module.id)
                .0
                .into_nc();

            let event = OutgoingPaymentStarted {
                operation_start: now(),
                outgoing_contract: payload.contract.clone(),
                min_contract_amount,
                invoice_amount: Amount::from_msats(amount),
                max_delay,
                destination_node: Some(invoice.get_payee_pub_key()),
            };

            f1_module
                .log_send_started_dbtx(&mut f1_module_dbtx, operation_id, event)
                .await;
        }

        // --- Direct-swap vs external LN ------------------------------------

        if self.node.node_id() == invoice.get_payee_pub_key() {
            // Direct swap: the invoice was issued by this gateway, so a
            // registered incoming contract is the payment's target. Fund it;
            // the receive trailer finalizes the send once the receive settles.
            let incoming_row = dbtx
                .get_value(&IncomingContractKey(operation_id))
                .await
                .expect("Direct-swap target not registered for this payment hash");

            ensure!(
                incoming_row.amount.msats == amount,
                "Direct-swap amount mismatch"
            );

            let f2_client = self
                .select_client(incoming_row.federation_id)
                .await
                .expect("Direct-swap target federation not connected");

            let f2_module = f2_client
                .get_first_module::<GatewayClientModuleV2>()
                .expect("Must have client module");

            let client_operation_id = OperationId::from_encodable(&incoming_row.contract);

            let f2_client_dbtx =
                dbtx.to_ref_with_prefix(client_db_prefix(&incoming_row.federation_id));

            let mut f2_module_dbtx = f2_client_dbtx
                .with_prefix_module_id(f2_module.id)
                .0
                .into_nc();

            let start_receive = f2_module
                .start_receive_dbtx(
                    &mut f2_module_dbtx,
                    client_operation_id,
                    incoming_row.contract,
                    amount,
                )
                .await;

            // Release the F2 view's borrow of `dbtx` so the cancellation arm
            // can derive the F1 view.
            drop(f2_module_dbtx);

            if let Err(err) = start_receive {
                let f1_client_dbtx =
                    dbtx.to_ref_with_prefix(client_db_prefix(&payload.federation_id));

                let mut f1_module_dbtx = f1_client_dbtx
                    .with_prefix_module_id(f1_module.id)
                    .0
                    .into_nc();

                f1_module
                    .finalize_send_dbtx(
                        &mut f1_module_dbtx,
                        operation_id,
                        payload.contract,
                        payload.outpoint,
                        Err(Cancelled::FinalizationError(err.to_string())),
                    )
                    .await;
            }
        } else {
            // External LN payment, fire-and-forget: the outcome arrives via
            // the LDK `PaymentSuccessful` / `PaymentFailed` events.
            let params = lightning::routing::router::RouteParametersConfig {
                max_total_routing_fee_msat: Some(max_fee.msats),
                max_total_cltv_expiry_delta: max_delay as u32,
                ..Default::default()
            };

            // A duplicate payment means a previous run of this request already
            // kicked off the payment (its transaction failed to commit after
            // the LDK send); the LDK events drive its terminal, so treat it as
            // a successful kick-off instead of cancelling an in-flight send.
            let send_result = match self.node.bolt11_payment().send(&invoice, Some(params)) {
                Err(ldk_node::NodeError::DuplicatePayment) => Ok(()),
                result => result.map(|_| ()),
            };

            if let Err(err) = send_result {
                let f1_client_dbtx =
                    dbtx.to_ref_with_prefix(client_db_prefix(&payload.federation_id));

                let mut f1_module_dbtx = f1_client_dbtx
                    .with_prefix_module_id(f1_module.id)
                    .0
                    .into_nc();

                f1_module
                    .finalize_send_dbtx(
                        &mut f1_module_dbtx,
                        operation_id,
                        payload.contract,
                        payload.outpoint,
                        Err(Cancelled::LightningRpcError(err.to_string())),
                    )
                    .await;
            }
        }

        dbtx.commit_tx().await;

        // --- Await the terminal event on the source federation ------------

        Self::subscribe_send(&f1_client, operation_id).await
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
    /// `Ok((preimage, ln_fee))` or forfeits it with `Err(error)`. The realized
    /// Lightning routing fee is zero on the direct-swap path.
    pub(crate) async fn finalize_send_for(
        &self,
        federation_id: FederationId,
        contract: fedimint_lnv2_common::contracts::OutgoingContract,
        outpoint: fedimint_core::OutPoint,
        outcome: Result<([u8; 32], Amount), Cancelled>,
    ) {
        let PaymentImage::Hash(payment_hash) = contract.payment_image else {
            warn!(target: LOG_GATEWAY, "Outgoing contract has no payment hash");
            return;
        };

        let operation_id = OperationId::from_encodable(&payment_hash);

        self.select_client(federation_id)
            .await
            .expect("source federation for outgoing contract is joined")
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module")
            .finalize_send(operation_id, contract, outpoint, outcome)
            .await;
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
            amount: payload.amount,
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
}

// LDK event loop
impl AppState {
    /// Drives the LDK node's event queue until the task is aborted (on process
    /// shutdown). Inbound payments become claimable here; outbound payment
    /// outcomes finalize their outgoing contracts. Each event is acknowledged
    /// with `event_handled` before the next is pulled. Modeled on picomint's
    /// `process_ldk_events`.
    pub async fn process_ldk_events(self) {
        info!(target: LOG_GATEWAY, "Gateway is running");
        loop {
            let event = self.node.next_event_async().await;

            // One write transaction per event, picomint-style: the
            // once-per-event marker and the event's client-side work commit
            // atomically. Commits race the federation clients' state machine
            // executors (optimistic concurrency), so the transaction is
            // retried via autocommit. The handlers' LDK side effect
            // (`fail_for_hash`) is idempotent, so re-running the closure is
            // safe.
            self.gateway_db
                .autocommit(
                    |dbtx, _| {
                        let event = event.clone();
                        let this = &self;
                        Box::pin(async move {
                            this.process_ldk_event(dbtx, event).await;
                            Ok::<_, Infallible>(())
                        })
                    },
                    Some(100),
                )
                .await
                .expect("Failed to commit LDK event dbtx");

            if let Err(err) = self.node.event_handled() {
                warn!(target: LOG_GATEWAY, err = %err.fmt_compact(), "LDK could not mark event handled");
            }
        }
    }

    /// Processes one LDK event inside the caller's write transaction.
    async fn process_ldk_event(&self, dbtx: &mut DatabaseTransaction<'_>, event: ldk_node::Event) {
        match event {
            ldk_node::Event::PaymentClaimable {
                payment_hash,
                claimable_amount_msat,
                ..
            } => {
                let payment_hash = sha256::Hash::from_byte_array(payment_hash.0);
                self.handle_payment_claimable(dbtx, payment_hash, claimable_amount_msat)
                    .await;
            }
            ldk_node::Event::PaymentSuccessful {
                payment_hash,
                payment_preimage: Some(preimage),
                fee_paid_msat,
                ..
            } => {
                let payment_hash = sha256::Hash::from_byte_array(payment_hash.0);
                self.handle_payment_terminal(
                    dbtx,
                    payment_hash,
                    Ok((preimage.0, Amount::from_msats(fee_paid_msat.unwrap_or(0)))),
                )
                .await;
            }
            ldk_node::Event::PaymentFailed {
                payment_hash: Some(payment_hash),
                reason,
                ..
            } => {
                let payment_hash = sha256::Hash::from_byte_array(payment_hash.0);
                let error = Cancelled::LightningRpcError(reason.map_or_else(
                    || "unknown failure reason".to_string(),
                    |r| format!("{r:?}"),
                ));
                self.handle_payment_terminal(dbtx, payment_hash, Err(error))
                    .await;
            }
            _ => {}
        }
    }

    /// Handles an inbound Lightning payment that the LDK node reports as
    /// claimable. If it matches the issued invoice's amount, submits the
    /// incoming-contract funding tx and spawns the federation-local Receive
    /// state machine (via [`start_receive_dbtx`]); on amount mismatch or
    /// funding failure (e.g. insufficient gateway liquidity) fails the HTLC
    /// back so the sender is refunded promptly.
    ///
    /// Note selection fails before any client-side write, so on failure the
    /// transaction still commits holding only the [`ProcessedLdkEventKey`]
    /// marker: each payment hash gets one shot, as in picomint.
    ///
    /// The upstream HTLC is *not* claimed here — that is driven out-of-band by
    /// the per-client receive trailer once the Receive SM reaches success.
    ///
    /// [`start_receive_dbtx`]: fedimint_gwv2_client::GatewayClientModuleV2::start_receive_dbtx
    async fn handle_payment_claimable(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        payment_hash: sha256::Hash,
        amount_msat: u64,
    ) {
        if dbtx
            .insert_entry(&ProcessedLdkEventKey(payment_hash.to_byte_array()), &())
            .await
            .is_some()
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        // LDK only fires PaymentClaimable for hashes we registered via
        // `receive_for_hash` in `AppState::receive`, which commits the
        // IncomingContract row before returning the invoice.
        let registered = dbtx
            .get_value(&IncomingContractKey(operation_id))
            .await
            .expect("PaymentClaimable for an unregistered payment_hash");

        // The HTLC pays the invoice amount, which is the contract amount plus
        // the gateway's receive fee — not `contract.commitment.amount`.
        if registered.amount.msats != amount_msat {
            warn!(
                target: LOG_GATEWAY,
                %payment_hash,
                "Claimable payment amount does not match the issued invoice; failing HTLC"
            );
            self.fail_for_hash(payment_hash);
            return;
        }

        let client = self
            .select_client(registered.federation_id)
            .await
            .expect("source federation for incoming contract is joined");

        let module = client
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module");

        // The client-side operation is keyed by the contract (matching
        // `relay_direct_swap`, which the Send state machine uses for direct
        // swaps), so both receive paths land on the same operation.
        let client_operation_id = OperationId::from_encodable(&registered.contract);

        let client_dbtx = dbtx.to_ref_with_prefix(client_db_prefix(&registered.federation_id));

        let mut module_dbtx = client_dbtx.with_prefix_module_id(module.id).0;

        if module
            .start_receive_dbtx(
                &mut module_dbtx,
                client_operation_id,
                registered.contract,
                amount_msat,
            )
            .await
            .is_err()
        {
            // Funding the incoming contract failed (e.g. the gateway is not
            // pegged in / has insufficient liquidity). Fail the HTLC back so
            // the sender is refunded, matching the v1 relay-error path.
            self.fail_for_hash(payment_hash);
        }
    }

    /// Handles a terminal outbound LN payment: looks up the outgoing contract
    /// row and finalizes the send on the source federation — claiming the
    /// contract with the preimage and realized routing fee carried on
    /// `PaymentSuccessful`, or forfeiting it on `PaymentFailed` so the sender
    /// is refunded. Payments without a row (e.g. operator-initiated sends)
    /// are just marked processed.
    ///
    /// A lost terminal would forfeit the gateway's own claim on the outgoing
    /// contract, so failures here are invariant violations and panic; the
    /// aborted transaction leaves no marker and LDK replays the event after
    /// the restart.
    async fn handle_payment_terminal(
        &self,
        dbtx: &mut DatabaseTransaction<'_>,
        payment_hash: sha256::Hash,
        outcome: Result<([u8; 32], Amount), Cancelled>,
    ) {
        if dbtx
            .insert_entry(&ProcessedLdkEventKey(payment_hash.to_byte_array()), &())
            .await
            .is_some()
        {
            return;
        }

        let operation_id = OperationId::from_encodable(&payment_hash);

        let Some(row) = dbtx.get_value(&OutgoingContractKey(operation_id)).await else {
            return;
        };

        let client = self
            .select_client(row.federation_id)
            .await
            .expect("source federation for outgoing contract is joined");

        let module = client
            .get_first_module::<GatewayClientModuleV2>()
            .expect("Must have client module");

        let client_dbtx = dbtx.to_ref_with_prefix(client_db_prefix(&row.federation_id));

        let mut module_dbtx = client_dbtx.with_prefix_module_id(module.id).0;

        module
            .finalize_send_dbtx(
                &mut module_dbtx,
                operation_id,
                row.contract,
                row.outpoint,
                outcome,
            )
            .await;
    }
}

// Lightning node access shared by the LNv2 protocol, the trailer, and the
// admin CLI. (Operator-only node management lives directly in [`crate::cli`],
// picomint-style.)
impl AppState {
    /// Claims (settles) a claimable inbound HTLC on the lightning node with the
    /// given `preimage`. Called by the receive trailer once the federation-side
    /// receive succeeds.
    pub(crate) async fn claim_for_hash(&self, payment_hash: sha256::Hash, preimage: [u8; 32]) {
        let registered = self
            .gateway_db
            .begin_transaction_nc()
            .await
            .get_value(&IncomingContractKey(OperationId::from_encodable(
                &payment_hash,
            )))
            .await
            .expect("incoming contract row registered by AppState::receive");

        self.node
            .bolt11_payment()
            .claim_for_hash(
                PaymentHash(*payment_hash.as_byte_array()),
                registered.amount.msats,
                PaymentPreimage(preimage),
            )
            .expect("LDK has this payment_hash (registered via receive_for_hash)");
    }

    /// Fails a claimable inbound HTLC back to the sender (refund).
    pub(crate) fn fail_for_hash(&self, payment_hash: sha256::Hash) {
        let ph = PaymentHash(*payment_hash.as_byte_array());

        self.node
            .bolt11_payment()
            .fail_for_hash(ph)
            .expect("LDK has this payment_hash (registered via receive_for_hash)");
    }
}
