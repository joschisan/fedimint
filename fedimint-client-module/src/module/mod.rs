use core::fmt;
use std::any::Any;
use std::fmt::Debug;
use std::sync::{Arc, Weak};
use std::{marker, ops};

use anyhow::bail;
use bitcoin::secp256k1::PublicKey;
use fedimint_api_client::api::{DynGlobalApi, DynModuleApi};
use fedimint_core::config::ClientConfig;
use fedimint_core::core::{
    Decoder, DynInput, DynOutput, IInput, IntoDynInstance, ModuleInstanceId, ModuleKind,
    OperationId,
};
use fedimint_core::invite_code::InviteCode;
use fedimint_core::module::registry::{ModuleDecoderRegistry, ModuleRegistry};
use fedimint_core::module::{CommonModuleInit, ModuleCommon, ModuleInit};
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::util::{BoxFuture, BoxStream};
use fedimint_core::{
    Amount, PeerId, TransactionId, apply, async_trait_maybe_send, dyn_newtype_define,
    maybe_add_send_sync,
};
use fedimint_eventlog::{Event, EventKind, EventLogId, PersistedLogEntry};
use fedimint_logging::LOG_CLIENT;
use fedimint_redb::{Database, WriteTxRef};
use tokio::sync::watch;
use tracing::warn;

use self::init::ClientModuleInit;
use crate::InstancelessDynClientInputBundle;
use crate::transaction::{ClientInputBundle, ClientOutputBundle, TransactionBuilder};

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
    maybe_add_send_sync!(
        dyn for<'a> FnOnce(
                &'a WriteTxRef<'_>,
                TransactionId,
                IdxRange,
                IdxRange,
            ) -> BoxFuture<'a, ()>
            + 'static
    ),
>;

/// Same as [`FinalContribution`] but with type-erased inputs/outputs, for
/// the dyn pipeline.
pub struct DynFinalContribution {
    pub inputs: Vec<crate::transaction::ClientInput>,
    pub outputs: Vec<crate::transaction::ClientOutput>,
    pub spawn_sms: SpawnSms,
}

pub mod init;
pub mod recovery;

pub type ClientModuleRegistry = ModuleRegistry<DynClientModule>;

/// A fedimint-client interface exposed to client modules
///
/// To break the dependency of the client modules on the whole fedimint client
/// and in particular the `fedimint-client` crate, the module gets access to an
/// interface, that is implemented by the `Client`.
///
/// This allows lose coupling, less recompilation and better control and
/// understanding of what functionality of the Client the modules get access to.
#[apply(async_trait_maybe_send!)]
pub trait ClientContextIface: MaybeSend + MaybeSync {
    fn get_module(&self, instance: ModuleInstanceId) -> &maybe_add_send_sync!(dyn IClientModule);
    fn api_clone(&self) -> DynGlobalApi;
    fn decoders(&self) -> &ModuleDecoderRegistry;
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

    async fn await_tx_accepted(
        &self,
        operation_id: OperationId,
        txid: TransactionId,
    ) -> Result<(), String>;

    async fn config(&self) -> ClientConfig;

    fn db(&self) -> &Database;

    async fn invite_code(&self, peer: PeerId) -> Option<InviteCode>;

    fn get_internal_payment_markers(&self) -> anyhow::Result<(PublicKey, u64)>;

    fn log_event_added_rx(&self) -> watch::Receiver<()>;

    async fn get_event_log(&self, pos: Option<EventLogId>, limit: u64) -> Vec<PersistedLogEntry>;

    fn subscribe_operation_events(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, PersistedLogEntry>;

    #[allow(clippy::too_many_arguments)]
    async fn log_event_json(
        &self,
        dbtx: &WriteTxRef<'_>,
        module_kind: Option<ModuleKind>,
        module_id: ModuleInstanceId,
        kind: EventKind,
        operation_id: Option<OperationId>,
        payload: serde_json::Value,
    );
}

/// A final, fully initialized client
///
/// Client modules need to be able to access a `Client` they are a part
/// of. To break the circular dependency, the final `Client` is passed
/// after `Client` was built via a shared state.
#[derive(Clone, Default)]
pub struct FinalClientIface(Arc<std::sync::OnceLock<Weak<dyn ClientContextIface>>>);

impl FinalClientIface {
    /// Get a temporary strong reference to [`ClientContextIface`]
    ///
    /// Care must be taken to not let the user take ownership of this value,
    /// and not store it elsewhere permanently either, as it could prevent
    /// the cleanup of the Client.
    pub(crate) fn get(&self) -> Arc<dyn ClientContextIface> {
        self.0
            .get()
            .expect("client must be already set")
            .upgrade()
            .expect("client module context must not be use past client shutdown")
    }

    pub fn set(&self, client: Weak<dyn ClientContextIface>) {
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
/// Client modules can interact with the whole
/// client through this struct.
pub struct ClientContext<M> {
    client: FinalClientIface,
    module_instance_id: ModuleInstanceId,
    module_db: Database,
    _marker: marker::PhantomData<M>,
}

impl<M> Clone for ClientContext<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            module_db: self.module_db.clone(),
            module_instance_id: self.module_instance_id,
            _marker: marker::PhantomData,
        }
    }
}

/// A reference back to itself that the module cacn get from the
/// [`ClientContext`]
pub struct ClientContextSelfRef<'s, M> {
    // we are OK storing `ClientStrong` here, because of the `'s` preventing `Self` from being
    // stored permanently somewhere
    client: Arc<dyn ClientContextIface>,
    module_instance_id: ModuleInstanceId,
    _marker: marker::PhantomData<&'s M>,
}

impl<M> ops::Deref for ClientContextSelfRef<'_, M>
where
    M: ClientModule,
{
    type Target = M;

    fn deref(&self) -> &Self::Target {
        self.client
            .get_module(self.module_instance_id)
            .as_any()
            .downcast_ref::<M>()
            .unwrap_or_else(|| panic!("Module is not of type {}", std::any::type_name::<M>()))
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
    pub fn new(
        client: FinalClientIface,
        module_instance_id: ModuleInstanceId,
        module_db: Database,
    ) -> Self {
        Self {
            client,
            module_instance_id,
            module_db,
            _marker: marker::PhantomData,
        }
    }

    /// Get a reference back to client module from the [`Self`]
    ///
    /// It's often necessary for a client module to "move self"
    /// by-value, especially due to async lifetimes issues.
    /// Clients usually work with `&mut self`, which can't really
    /// work in such context.
    ///
    /// Fortunately [`ClientContext`] is `Clone` and `Send, and
    /// can be used to recover the reference to the module at later
    /// time.
    #[allow(clippy::needless_lifetimes)] // just for explicitiness
    pub fn self_ref(&self) -> ClientContextSelfRef<'_, M> {
        ClientContextSelfRef {
            client: self.client.get(),
            module_instance_id: self.module_instance_id,
            _marker: marker::PhantomData,
        }
    }

    /// Get a reference to a global Api handle
    pub fn global_api(&self) -> DynGlobalApi {
        self.client.get().api_clone()
    }

    /// Get a reference to a module Api handle
    pub fn module_api(&self) -> DynModuleApi {
        self.global_api().with_module(self.module_instance_id)
    }

    /// A set of all decoders of all modules of the client
    pub fn decoders(&self) -> ModuleDecoderRegistry {
        Clone::clone(self.client.get().decoders())
    }

    pub fn map_dyn<'s, 'i, 'o, I>(
        &'s self,
        typed: impl IntoIterator<Item = I> + 'i,
    ) -> impl Iterator<Item = <I as IntoDynInstance>::DynType> + 'o
    where
        I: IntoDynInstance,
        'i: 'o,
        's: 'o,
    {
        typed.into_iter().map(|i| self.make_dyn(i))
    }

    /// Turn a `typed` into a dyn version
    pub fn make_dyn<I>(&self, typed: I) -> <I as IntoDynInstance>::DynType
    where
        I: IntoDynInstance,
    {
        typed.into_dyn(self.module_instance_id)
    }

    /// Turn a typed [`ClientOutputBundle`] into a dyn version
    pub fn make_client_outputs<O>(&self, output: ClientOutputBundle<O>) -> ClientOutputBundle
    where
        O: fedimint_core::core::IOutput + MaybeSend + MaybeSync + 'static,
    {
        output.into_instanceless().into_dyn(self.module_instance_id)
    }

    /// Turn a typed [`ClientInputBundle`] into a dyn version
    pub fn make_client_inputs<I>(&self, inputs: ClientInputBundle<I>) -> ClientInputBundle
    where
        I: IInput + MaybeSend + MaybeSync + 'static,
    {
        inputs.into_instanceless().into_dyn(self.module_instance_id)
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
        txid: TransactionId,
    ) -> Result<(), String> {
        self.client
            .get()
            .await_tx_accepted(operation_id, txid)
            .await
    }

    pub async fn get_config(&self) -> ClientConfig {
        self.client.get().config().await
    }

    /// Returns an invite code for the federation that points to an arbitrary
    /// guardian server for fetching the config
    pub async fn get_invite_code(&self) -> InviteCode {
        let cfg = self.get_config().await.global;
        self.client
            .get()
            .invite_code(
                *cfg.api_endpoints
                    .keys()
                    .next()
                    .expect("A federation always has at least one guardian"),
            )
            .await
            .expect("The guardian we requested an invite code for exists")
    }

    pub fn get_internal_payment_markers(&self) -> anyhow::Result<(PublicKey, u64)> {
        self.client.get().get_internal_payment_markers()
    }

    pub async fn claim_inputs<I>(
        &self,
        dbtx: &WriteTxRef<'_>,
        inputs: ClientInputBundle<I>,
        operation_id: OperationId,
    ) -> anyhow::Result<OutPointRange>
    where
        I: IInput + MaybeSend + MaybeSync + 'static,
    {
        self.claim_inputs_dyn(dbtx, inputs.into_instanceless(), operation_id)
            .await
    }

    async fn claim_inputs_dyn(
        &self,
        dbtx: &WriteTxRef<'_>,
        inputs: InstancelessDynClientInputBundle,
        operation_id: OperationId,
    ) -> anyhow::Result<OutPointRange> {
        let tx_builder =
            TransactionBuilder::new().with_inputs(inputs.into_dyn(self.module_instance_id));

        self.client
            .get()
            .finalize_and_submit_transaction_inner(&dbtx.deisolate(), operation_id, tx_builder)
            .await
    }

    /// Watch channel that signals when any new event is added to the
    /// persistent event log.
    pub fn log_event_added_rx(&self) -> watch::Receiver<()> {
        self.client.get().log_event_added_rx()
    }

    /// Read a batch of persisted event log entries starting at `pos`.
    pub async fn get_event_log(
        &self,
        pos: Option<EventLogId>,
        limit: u64,
    ) -> Vec<PersistedLogEntry> {
        self.client.get().get_event_log(pos, limit).await
    }

    /// Stream every event belonging to `operation_id`, starting from the
    /// beginning of the log (existing events first, then live ones).
    pub fn subscribe_operation_events(
        &self,
        operation_id: OperationId,
    ) -> BoxStream<'static, PersistedLogEntry> {
        self.client.get().subscribe_operation_events(operation_id)
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
        use futures::StreamExt as _;
        Box::pin(
            self.subscribe_operation_events(operation_id)
                .filter_map(|entry| async move { entry.to_event::<E>() }),
        )
    }

    pub async fn log_event<E>(&self, dbtx: &WriteTxRef<'_>, operation_id: OperationId, event: E)
    where
        E: Event + Send,
    {
        if <E as Event>::MODULE != Some(<M as ClientModule>::kind()) {
            warn!(
                target: LOG_CLIENT,
                module_kind = %<M as ClientModule>::kind(),
                event_module = ?<E as Event>::MODULE,
                "Client module logging events of different module than its own. This might become an error in the future."
            );
        }
        self.client
            .get()
            .log_event_json(
                &dbtx.deisolate(),
                <E as Event>::MODULE,
                self.module_instance_id,
                <E as Event>::KIND,
                Some(operation_id),
                serde_json::to_value(event).expect("Can't fail"),
            )
            .await;
    }
}

/// Fedimint module client
#[apply(async_trait_maybe_send!)]
pub trait ClientModule: Debug + MaybeSend + MaybeSync + 'static {
    type Init: ClientModuleInit;

    /// Common module types shared between client and server
    type Common: ModuleCommon;

    fn decoder() -> Decoder {
        Self::Common::decoder_builder().build()
    }

    fn kind() -> ModuleKind {
        <<<Self as ClientModule>::Init as ModuleInit>::Common as CommonModuleInit>::KIND
    }

    /// Initialize client.
    ///
    /// Called by the core client code on start, after [`ClientContext`] is
    /// fully initialized, so unlike during [`ClientModuleInit::init`],
    /// access to global client is allowed.
    async fn start(&self) {}

    /// Returns the fee the processing of this input requires.
    ///
    /// If the semantics of a given input aren't known this function returns
    /// `None`, this only happens if a future version of Fedimint introduces a
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
    ///
    /// If the semantics of a given output aren't known this function returns
    /// `None`, this only happens if a future version of Fedimint introduces a
    /// new output variant. For clients this should only be the case when
    /// processing transactions created by other users, so the result of
    /// this function can be `unwrap`ped whenever dealing with inputs
    /// generated by ourselves.
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
    /// The function returns an error if and only if the client's funds are not
    /// sufficient to create the inputs necessary to fully fund the transaction.
    ///
    /// A returned input also contains:
    /// * A set of private keys belonging to the input for signing the
    ///   transaction
    /// * A closure that generates states belonging to the input. This closure
    ///   takes the transaction id of the transaction in which the input was
    ///   used and the input index as input since these cannot be known at time
    ///   of calling `create_funding_input` and have to be injected later.
    ///
    /// A returned output also contains:
    /// * A closure that generates states belonging to the output. This closure
    ///   takes the transaction id of the transaction in which the output was
    ///   used and the output index as input since these cannot be known at time
    ///   of calling `create_change_output` and have to be injected later.
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

    /// Leave the federation
    ///
    /// While technically there's nothing stopping the client from just
    /// abandoning Federation at any point by deleting all the related
    /// local data, it is useful to make sure it's safe beforehand.
    ///
    /// This call indicates the desire of the caller client code
    /// to orderly and safely leave the Federation by this module instance.
    /// The goal of the implementations is to fulfil that wish,
    /// giving prompt and informative feedback if it's not yet possible.
    ///
    /// The client module implementation should handle the request
    /// and return as fast as possible avoiding blocking for longer than
    /// necessary. This would usually involve some combination of:
    ///
    /// * recording the state of being in process of leaving the Federation to
    ///   prevent initiating new conditions that could delay its completion;
    /// * performing any fast to complete cleanup/exit logic;
    /// * initiating any time-consuming logic (e.g. canceling outstanding
    ///   contracts), as background jobs, tasks machines, etc.
    /// * checking for any conditions indicating it might not be safe to leave
    ///   at the moment.
    ///
    /// This function should return `Ok` only if from the perspective
    /// of this module instance, it is safe to delete client data and
    /// stop using it, with no further actions (like background jobs) required
    /// to complete.
    ///
    /// This function should return an error if it's not currently possible
    /// to safely (e.g. without losing funds) leave the Federation.
    /// It should avoid running indefinitely trying to complete any cleanup
    /// actions necessary to reach a clean state, preferring spawning new
    /// state machines and returning an informative error about cleanup
    /// still in progress.
    ///
    /// If any internal task needs to complete, any user action is required,
    /// or even external condition needs to be met this function
    /// should return a `Err`.
    ///
    /// Notably modules should not disable interaction that might be necessary
    /// for the user (possibly through other modules) to leave the Federation.
    /// In particular a Mint module should retain ability to create new notes,
    /// and LN module should retain ability to send funds out.
    ///
    /// Calling code must NOT assume that a module that once returned `Ok`,
    /// will not return `Err` at later point. E.g. a Mint module might have
    /// no outstanding balance at first, but other modules winding down
    /// might "cash-out" to Ecash.
    ///
    /// Before leaving the Federation and deleting any state the calling code
    /// must collect a full round of `Ok` from all the modules.
    ///
    /// Calling code should allow the user to override and ignore any
    /// outstanding errors, after sufficient amount of warnings. Ideally,
    /// this should be done on per-module basis, to avoid mistakes.
    async fn leave(&self, _dbtx: &WriteTxRef<'_>) -> anyhow::Result<()> {
        bail!("Unable to determine if safe to leave the federation: Not implemented")
    }
}

/// Type-erased version of [`ClientModule`]
#[apply(async_trait_maybe_send!)]
pub trait IClientModule: Debug {
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn std::any::Any));

    fn decoder(&self) -> Decoder;

    async fn start(&self);

    fn input_fee(&self, amount: Amount, input: &DynInput) -> Option<Amount>;

    fn output_fee(&self, amount: Amount, output: &DynOutput) -> Option<Amount>;

    fn supports_being_primary(&self) -> bool;

    async fn create_final_inputs_and_outputs(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        input_amount: Amount,
        output_amount: Amount,
    ) -> anyhow::Result<DynFinalContribution>;

    async fn get_balance(&self, module_instance: ModuleInstanceId, dbtx: &WriteTxRef<'_>)
    -> Amount;

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()>;
}

#[apply(async_trait_maybe_send!)]
impl<T> IClientModule for T
where
    T: ClientModule,
{
    fn as_any(&self) -> &(maybe_add_send_sync!(dyn Any)) {
        self
    }

    fn decoder(&self) -> Decoder {
        T::decoder()
    }

    async fn start(&self) {
        <T as ClientModule>::start(self).await;
    }

    fn input_fee(&self, amount: Amount, input: &DynInput) -> Option<Amount> {
        <T as ClientModule>::input_fee(
            self,
            amount,
            input
                .as_any()
                .downcast_ref()
                .expect("Dispatched to correct module"),
        )
    }

    fn output_fee(&self, amount: Amount, output: &DynOutput) -> Option<Amount> {
        <T as ClientModule>::output_fee(
            self,
            amount,
            output
                .as_any()
                .downcast_ref()
                .expect("Dispatched to correct module"),
        )
    }

    fn supports_being_primary(&self) -> bool {
        <T as ClientModule>::supports_being_primary(self)
    }

    async fn create_final_inputs_and_outputs(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &WriteTxRef<'_>,
        operation_id: OperationId,
        input_amount: Amount,
        output_amount: Amount,
    ) -> anyhow::Result<DynFinalContribution> {
        let FinalContribution {
            inputs,
            outputs,
            spawn_sms,
        } = <T as ClientModule>::create_final_inputs_and_outputs(
            self,
            &dbtx.isolate(format!("module-{module_instance}")),
            operation_id,
            input_amount,
            output_amount,
        )
        .await?;

        let inputs = inputs
            .into_iter()
            .map(|i| i.into_dyn(module_instance))
            .collect();
        let outputs = outputs
            .into_iter()
            .map(|o| o.into_dyn(module_instance))
            .collect();

        Ok(DynFinalContribution {
            inputs,
            outputs,
            spawn_sms,
        })
    }

    async fn get_balance(
        &self,
        module_instance: ModuleInstanceId,
        dbtx: &WriteTxRef<'_>,
    ) -> Amount {
        <T as ClientModule>::get_balance(self, &dbtx.isolate(format!("module-{module_instance}")))
            .await
    }

    async fn subscribe_balance_changes(&self) -> BoxStream<'static, ()> {
        <T as ClientModule>::subscribe_balance_changes(self).await
    }
}

dyn_newtype_define!(
    #[derive(Clone)]
    pub DynClientModule(Arc<IClientModule>)
);

impl AsRef<maybe_add_send_sync!(dyn IClientModule + 'static)> for DynClientModule {
    fn as_ref(&self) -> &maybe_add_send_sync!(dyn IClientModule + 'static) {
        self.inner.as_ref()
    }
}

// Re-export types from fedimint_core
pub use fedimint_core::{IdxRange, OutPointRange, OutPointRangeIter};

pub type StateGenerator<S> = Arc<maybe_add_send_sync!(dyn Fn(OutPointRange) -> Vec<S> + 'static)>;
