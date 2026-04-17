use core::fmt;
use std::fmt::Debug;
use std::marker;
use std::sync::{Arc, Weak};

use futures::StreamExt as _;
use picomint_api_client::api::{ApiScope, FederationApi};
use picomint_api_client::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::core::{ModuleKind, OperationId};
use picomint_core::invite_code::InviteCode;
use picomint_core::module::{CommonModuleInit, ModuleCommon, ModuleInit};
use picomint_core::util::{BoxFuture, BoxStream};
use picomint_core::{Amount, TransactionId};
use picomint_eventlog::{Event, EventLogId, PersistedLogEntry};
use picomint_logging::LOG_CLIENT;
use picomint_redb::{Database, WriteTxRef};
use tokio::sync::watch;
use tracing::warn;

use self::init::ClientModuleInit;
use crate::transaction::{ClientInputBundle, ClientOutputBundle, TransactionBuilder};
use crate::{TxAcceptedEvent, TxRejectedEvent};

/// Return type of [`ClientModule::create_final_inputs_and_outputs`]. The
/// primary module contributes inputs/outputs to balance a partial
/// transaction and — once the final txid + index ranges are known —
/// spawns any state machines it needs to track those contributions.
///
/// `spawn_sms` is invoked exactly once by the submission path, *after*
/// the txid is computed, with the range of input indices and range of
/// output indices allocated to the primary contribution.
pub struct FinalContribution<I, O> {
    pub inputs: Vec<crate::transaction::ClientInput<I>>,
    pub outputs: Vec<crate::transaction::ClientOutput<O>>,
    pub spawn_sms: SpawnSms,
}

pub type SpawnSms = Box<
    dyn for<'a> FnOnce(&'a WriteTxRef<'_>, TransactionId, IdxRange, IdxRange) -> BoxFuture<'a, ()>
        + 'static
        + Send
        + Sync,
>;

pub mod init;
pub mod recovery;

/// Minimal back-reference trait a client module needs to finalize and submit
/// transactions. This is the only piece of the `Client` that modules cannot
/// compute from the values threaded into [`ClientContext`] at construction
/// time, because balancing a transaction requires access to the concrete
/// mint / ln / wallet modules to compute fees and generate change.
#[async_trait::async_trait]
pub trait FinalizeTransaction: Send + Sync {
    async fn finalize_and_submit_transaction(
        &self,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange>;

    async fn finalize_and_submit_transaction_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange>;

    async fn finalize_and_submit_transaction_inner(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange>;
}

/// Lazily-set, `Weak<dyn FinalizeTransaction>` handle used to break the
/// circular dependency between `picomint-client` and module crates. Set
/// exactly once by `ClientBuilder` after the `Client` is constructed.
#[derive(Clone, Default)]
pub struct FinalClientIface(Arc<std::sync::OnceLock<Weak<dyn FinalizeTransaction>>>);

impl FinalClientIface {
    /// Get a temporary strong reference.
    ///
    /// Care must be taken to not let the user take ownership of this value,
    /// and not store it elsewhere permanently either, as it could prevent
    /// the cleanup of the Client.
    pub(crate) fn get(&self) -> Arc<dyn FinalizeTransaction> {
        self.0
            .get()
            .expect("client must be already set")
            .upgrade()
            .expect("client module context must not be use past client shutdown")
    }

    pub fn set(&self, client: Weak<dyn FinalizeTransaction>) {
        self.0.set(client).expect("FinalLazyClient already set");
    }
}

impl fmt::Debug for FinalClientIface {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FinalClientIface")
    }
}

/// A Client context for a [`ClientModule`] `M`
///
/// Client modules can interact with the whole client through this struct.
/// All concrete handles are stored directly; only the three transaction
/// finalize methods reach back to the full `Client` via
/// [`FinalClientIface`].
pub struct ClientContext<M> {
    client: FinalClientIface,
    kind: ModuleKind,
    api: FederationApi,
    api_scope: ApiScope,
    db: Database,
    module_db: Database,
    config: ConsensusConfig,
    federation_id: FederationId,
    log_event_added_tx: watch::Sender<()>,
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
            log_event_added_tx: self.log_event_added_tx.clone(),
            _marker: marker::PhantomData,
        }
    }
}

impl<M> fmt::Debug for ClientContext<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ClientContext")
    }
}

impl<M> ClientContext<M>
where
    M: ClientModule,
{
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client: FinalClientIface,
        kind: ModuleKind,
        api: FederationApi,
        api_scope: ApiScope,
        db: Database,
        module_db: Database,
        config: ConsensusConfig,
        federation_id: FederationId,
        log_event_added_tx: watch::Sender<()>,
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
            log_event_added_tx,
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
        picomint_api_client::wire::Output: From<O>,
    {
        output.into_wire()
    }

    /// Lift a typed [`ClientInputBundle`] into a wire-level one.
    pub fn make_client_inputs<I>(&self, inputs: ClientInputBundle<I>) -> ClientInputBundle
    where
        picomint_api_client::wire::Input: From<I>,
    {
        inputs.into_wire()
    }

    pub async fn finalize_and_submit_transaction(
        &self,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        self.client
            .get()
            .finalize_and_submit_transaction(operation_id, tx_builder)
            .await
    }

    pub async fn finalize_and_submit_transaction_dbtx(
        &self,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        tx_builder: TransactionBuilder,
    ) -> anyhow::Result<OutPointRange> {
        self.client
            .get()
            .finalize_and_submit_transaction_dbtx(&dbtx.deisolate(), operation_id, tx_builder)
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
            if let Some(ev) = entry.to_event::<TxAcceptedEvent>()
                && ev.txid == query_txid
            {
                return Ok(());
            }
            if let Some(ev) = entry.to_event::<TxRejectedEvent>()
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
    ) -> anyhow::Result<OutPointRange>
    where
        picomint_api_client::wire::Input: From<I>,
    {
        let tx_builder = TransactionBuilder::new().with_inputs(inputs.into_wire());

        self.client
            .get()
            .finalize_and_submit_transaction_inner(&dbtx.deisolate(), operation_id, tx_builder)
            .await
    }

    /// Watch channel that signals when any new event is added to the
    /// persistent event log.
    pub fn log_event_added_rx(&self) -> watch::Receiver<()> {
        self.log_event_added_tx.subscribe()
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
            self.log_event_added_tx.subscribe(),
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
        picomint_eventlog::log_event(
            &dbtx.deisolate(),
            self.log_event_added_tx.clone(),
            Some(operation_id),
            event,
        );
    }
}

/// Picomint module client
#[async_trait::async_trait]
pub trait ClientModule: Debug + Send + Sync + 'static {
    type Init: ClientModuleInit;

    /// Common module types shared between client and server
    type Common: ModuleCommon;

    fn kind() -> ModuleKind {
        <<<Self as ClientModule>::Init as ModuleInit>::Common as CommonModuleInit>::KIND
    }

    /// Called by the core client code on start, after [`ClientContext`] is
    /// fully initialized, so unlike during [`ClientModuleInit::init`],
    /// access to global client is allowed.
    async fn start(&self) {}

    /// Returns the fee the processing of this input requires.
    ///
    /// If the semantics of a given input aren't known this function returns
    /// `None`, this only happens if a future version of Picomint introduces a
    /// new input variant. For clients this should only be the case when
    /// processing transactions created by other users, so the result of
    /// this function can be `unwrap`ped whenever dealing with inputs
    /// generated by ourselves.
    fn input_fee(
        &self,
        amount: Amount,
        input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amount>;

    /// Returns the fee the processing of this output requires.
    fn output_fee(
        &self,
        amount: Amount,
        output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amount>;

    /// Does this module support being a primary module
    ///
    /// If it does it must implement:
    ///
    /// * [`Self::create_final_inputs_and_outputs`]
    /// * [`Self::get_balance`]
    /// * [`Self::subscribe_balance_changes`]
    fn supports_being_primary(&self) -> bool {
        false
    }

    /// Creates all inputs and outputs necessary to balance the transaction.
    async fn create_final_inputs_and_outputs(
        &self,
        _dbtx: &WriteTxRef<'_>,
        _operation_id: OperationId,
        _input_amount: Amount,
        _output_amount: Amount,
    ) -> anyhow::Result<
        FinalContribution<
            <Self::Common as ModuleCommon>::Input,
            <Self::Common as ModuleCommon>::Output,
        >,
    > {
        unimplemented!()
    }

    /// Returns the balance held by this module and available for funding
    /// transactions.
    async fn get_balance(&self, _dbtx: &WriteTxRef<'_>) -> Amount {
        unimplemented!()
    }

    /// Returns a stream that will output the updated module balance each time
    /// it changes.
    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        unimplemented!()
    }
}

// Re-export types from picomint_core
pub use picomint_core::{IdxRange, OutPointRange, OutPointRangeIter};

pub type StateGenerator<S> = Arc<dyn Fn(OutPointRange) -> Vec<S> + 'static + Send + Sync>;
