use core::fmt;
use std::marker;
use std::sync::{Arc, OnceLock, Weak};

use futures::StreamExt as _;
use crate::Client;
use crate::api::{ApiScope, FederationApi};
use picomint_core::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::core::{ModuleKind, OperationId};
use picomint_core::invite_code::InviteCode;
use picomint_core::util::{BoxFuture, BoxStream};
use picomint_core::TransactionId;
use picomint_eventlog::{EVENT_LOG, Event, EventLogId, PersistedLogEntry};
use picomint_logging::LOG_CLIENT;
use picomint_redb::{Database, WriteTxRef};
use tokio::sync::Notify;
use tracing::warn;

use crate::transaction::{ClientInputBundle, ClientOutputBundle, TransactionBuilder};
use crate::{TxAcceptEvent, TxRejectEvent};

/// Late-bound weak reference to the owning `Client`, shared between
/// `ClientContext`s and set once by the builder after the `Client` is
/// constructed.
pub(crate) type LateClient = Arc<OnceLock<Weak<Client>>>;

/// Return type of [`ClientModule::create_final_inputs_and_outputs`]. The
/// primary module contributes inputs/outputs to balance a partial
/// transaction and — once the final txid is known — spawns any state
/// machines it needs to track those contributions.
///
/// `spawn_sms` is invoked exactly once by the submission path, *after*
/// the txid is computed.
pub struct FinalContribution<I, O> {
    pub inputs: Vec<crate::transaction::ClientInput<I>>,
    pub outputs: Vec<crate::transaction::ClientOutput<O>>,
    pub spawn_sms: SpawnSms,
}

pub type SpawnSms = Box<
    dyn for<'a> FnOnce(&'a WriteTxRef<'_>, TransactionId) -> BoxFuture<'a, ()>
        + 'static
        + Send
        + Sync,
>;

/// A client context for a module `M`.
///
/// Client modules can interact with the whole client through this struct.
/// All concrete handles are stored directly; only the transaction finalize
/// methods reach back to the full `Client` via a late-bound [`LateClient`].
pub struct ClientContext<M> {
    client: LateClient,
    kind: ModuleKind,
    api: FederationApi,
    api_scope: ApiScope,
    db: Database,
    module_db: Database,
    config: ConsensusConfig,
    federation_id: FederationId,
    _marker: marker::PhantomData<M>,
}

impl<M> Clone for ClientContext<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            kind: self.kind,
            api: self.api.clone(),
            api_scope: self.api_scope,
            db: self.db.clone(),
            module_db: self.module_db.clone(),
            config: self.config.clone(),
            federation_id: self.federation_id,
            _marker: marker::PhantomData,
        }
    }
}

impl<M> fmt::Debug for ClientContext<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientContext")
    }
}

impl<M> ClientContext<M> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: LateClient,
        kind: ModuleKind,
        api: FederationApi,
        api_scope: ApiScope,
        db: Database,
        module_db: Database,
        config: ConsensusConfig,
        federation_id: FederationId,
    ) -> Self {
        Self {
            client,
            kind,
            api,
            api_scope,
            db,
            module_db,
            config,
            federation_id,
            _marker: marker::PhantomData,
        }
    }

    /// Get a reference to a global Api handle
    pub fn global_api(&self) -> FederationApi {
        self.api.clone()
    }

    /// Get a reference to a module Api handle
    pub fn module_api(&self) -> FederationApi {
        self.api.clone().with_scope(self.api_scope)
    }

    /// Lift a typed [`ClientOutputBundle`] into a wire-level one.
    pub fn make_client_outputs<O>(&self, output: ClientOutputBundle<O>) -> ClientOutputBundle
    where
        picomint_core::wire::Output: From<O>,
    {
        output.into_wire()
    }

    /// Lift a typed [`ClientInputBundle`] into a wire-level one.
    pub fn make_client_inputs<I>(&self, inputs: ClientInputBundle<I>) -> ClientInputBundle
    where
        picomint_core::wire::Input: From<I>,
    {
        inputs.into_wire()
    }

    fn client(&self) -> Arc<Client> {
        self.client
            .get()
            .expect("client must be set before contexts are used")
            .upgrade()
            .expect("client must outlive its module contexts")
    }

    pub async fn finalize_and_submit_transaction(
        &self,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<TransactionId> {
        self.client()
            .finalize_and_submit_transaction(operation_id, tx_builder)
            .await
    }

    pub async fn finalize_and_submit_transaction_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<TransactionId> {
        self.client()
            .finalize_and_submit_transaction_dbtx(&dbtx.deisolate(), operation_id, tx_builder)
            .await
    }

    /// Submit an already-funded/balanced [`TransactionBuilder`] directly,
    /// bypassing the primary module's `create_final_inputs_and_outputs`. The
    /// caller is responsible for every input and output.
    pub async fn submit_tx_builder_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<TransactionId> {
        self.client()
            .submit_tx_builder_dbtx(&dbtx.deisolate(), operation_id, tx_builder)
            .await
    }

    pub fn module_db(&self) -> &Database {
        &self.module_db
    }

    pub async fn await_tx_accepted(
        &self,
        operation_id: OperationId,
        query_txid: TransactionId,
    ) -> Result<(), String> {
        let mut stream = self.subscribe_operation_events(operation_id);
        while let Some(entry) = stream.next().await {
            if let Some(ev) = entry.to_event::<TxAcceptEvent>()
                && ev.txid == query_txid
            {
                return Ok(());
            }
            if let Some(ev) = entry.to_event::<TxRejectEvent>()
                && ev.txid == query_txid
            {
                return Err(ev.error);
            }
        }
        unreachable!("subscribe_operation_events only ends at client shutdown")
    }

    pub fn get_config(&self) -> &ConsensusConfig {
        &self.config
    }

    pub fn federation_id(&self) -> FederationId {
        self.federation_id
    }

    /// Returns an invite code for the federation that points to an arbitrary
    /// guardian server for fetching the config
    pub fn get_invite_code(&self) -> InviteCode {
        let (peer, endpoints) = self
            .config
            .iroh_endpoints
            .iter()
            .next()
            .expect("A federation always has at least one guardian");
        InviteCode::new(endpoints.node_id, *peer, self.federation_id)
    }

    pub async fn claim_inputs<I>(
        &self,
        dbtx: &WriteTxRef<'_>,
        inputs: ClientInputBundle<I>,
        operation_id: OperationId,
    ) -> anyhow::Result<TransactionId>
    where
        picomint_core::wire::Input: From<I>,
    {
        let tx_builder = TransactionBuilder::new().with_inputs(inputs.into_wire());

        self.client()
            .finalize_and_submit_transaction_inner(&dbtx.deisolate(), operation_id, tx_builder)
            .await
    }

    /// Shared [`Notify`] that fires on every commit touching the event log.
    pub fn event_notify(&self) -> Arc<Notify> {
        self.db.notify_for_table(&EVENT_LOG)
    }

    /// Read a batch of persisted event log entries starting at `pos`.
    pub async fn get_event_log(
        &self,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        let pos = pos.unwrap_or(EventLogId::LOG_START);
        let end = pos.saturating_add(limit);
        self.db
            .begin_read()
            .await
            .as_ref()
            .with_native_table(&picomint_eventlog::EVENT_LOG, |t| {
                t.range(pos..end)
                    .expect("redb range failed")
                    .map(|r| {
                        let (k, v) = r.expect("redb range item failed");
                        PersistedLogEntry::new(k.value(), v.value())
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default()
    }

    /// Stream every event belonging to `operation_id`, starting from the
    /// beginning of the log (existing events first, then live ones).
    pub fn subscribe_operation_events(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, PersistedLogEntry> {
        Box::pin(picomint_eventlog::subscribe_operation_events(
            self.db.clone(),
            self.event_notify(),
            operation_id,
        ))
    }

    /// Typed variant of [`Self::subscribe_operation_events`] — yields only
    /// entries of kind `E`, decoded.
    pub fn subscribe_operation_events_typed<E>(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, E>
    where
        E: Event + Send + 'static,
    {
        Box::pin(
            self.subscribe_operation_events(operation_id)
                .filter_map(|entry| async move { entry.to_event::<E>() }),
        )
    }

    pub async fn log_event<E>(&self, dbtx: &WriteTxRef<'_>, operation_id: OperationId, event: E)
    where
        E: Event + Send,
    {
        if <E as Event>::MODULE != Some(self.kind) {
            warn!(
                target: LOG_CLIENT,
                module_kind = %self.kind,
                event_module = ?<E as Event>::MODULE,
                "Client module logging events of different module than its own. This might become an error in the future."
            );
        }
        picomint_eventlog::log_event(&dbtx.deisolate(), Some(operation_id), event);
    }
}


