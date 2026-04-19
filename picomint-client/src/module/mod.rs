use core::fmt;
use std::marker;
use std::sync::Arc;

use crate::api::{ApiScope, FederationApi};
use futures::StreamExt as _;
use picomint_core::TransactionId;
use picomint_core::config::ConsensusConfig;
use picomint_core::config::FederationId;
use picomint_core::core::{ModuleKind, OperationId};
use picomint_core::invite_code::InviteCode;
use picomint_core::util::BoxStream;
use picomint_eventlog::{EVENT_LOG, Event, EventLogId, PersistedLogEntry};
use picomint_logging::LOG_CLIENT;
use picomint_redb::{Database, WriteTxRef};
use tokio::sync::Notify;
use tracing::warn;

use crate::{TxAcceptEvent, TxRejectEvent};

/// A client context for a module `M`. Bundles the per-module API, db
/// handles, and federation config that module code reaches for via the
/// generic parameter.
pub struct ClientContext<M> {
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
        kind: ModuleKind,
        api: FederationApi,
        api_scope: ApiScope,
        db: Database,
        module_db: Database,
        config: ConsensusConfig,
        federation_id: FederationId,
    ) -> Self {
        Self {
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
