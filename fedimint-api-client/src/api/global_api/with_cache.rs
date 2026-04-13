use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;

use fedimint_core::core::ModuleInstanceId;
use fedimint_core::endpoint_constants::{
    AWAIT_TRANSACTION_ENDPOINT, LIVENESS_ENDPOINT, SUBMIT_TRANSACTION_ENDPOINT,
};
use fedimint_core::module::{ApiRequestErased, SerdeModuleEncoding};
use fedimint_core::task::{MaybeSend, MaybeSync};
use fedimint_core::transaction::{SerdeTransaction, Transaction, TransactionSubmissionOutcome};
use fedimint_core::{PeerId, TransactionId, apply, async_trait_maybe_send};
use futures::stream::BoxStream;
use serde_json::Value;

use super::super::{DynModuleApi, IGlobalFederationApi, IRawFederationApi};
use crate::api::{FederationApiExt, FederationResult};
use crate::connection::{DynGuaridianConnection, ServerResult};

/// Convenience extension trait used for wrapping [`IRawFederationApi`] in
/// a [`GlobalFederationApiWithCache`]
pub trait GlobalFederationApiWithCacheExt
where
    Self: Sized,
{
    fn with_cache(self) -> GlobalFederationApiWithCache<Self>;
}

impl<T> GlobalFederationApiWithCacheExt for T
where
    T: IRawFederationApi + MaybeSend + MaybeSync + 'static,
{
    fn with_cache(self) -> GlobalFederationApiWithCache<T> {
        GlobalFederationApiWithCache::new(self)
    }
}

/// [`IGlobalFederationApi`] wrapping some `T: IRawFederationApi` and adding
/// a thin layer.
///
/// Use [`GlobalFederationApiWithCacheExt::with_cache`] to create.
#[derive(Debug)]
pub struct GlobalFederationApiWithCache<T> {
    pub(crate) inner: T,
}

impl<T> GlobalFederationApiWithCache<T> {
    pub fn new(inner: T) -> GlobalFederationApiWithCache<T> {
        Self { inner }
    }
}

#[apply(async_trait_maybe_send!)]
impl<T> IRawFederationApi for GlobalFederationApiWithCache<T>
where
    T: IRawFederationApi + MaybeSend + MaybeSync + 'static,
{
    fn all_peers(&self) -> &BTreeSet<PeerId> {
        self.inner.all_peers()
    }

    fn self_peer(&self) -> Option<PeerId> {
        self.inner.self_peer()
    }

    fn with_module(&self, id: ModuleInstanceId) -> DynModuleApi {
        self.inner.with_module(id)
    }

    /// Make request to a specific federation peer by `peer_id`
    async fn request_raw(
        &self,
        peer_id: PeerId,
        method: &str,
        params: &ApiRequestErased,
    ) -> ServerResult<Value> {
        self.inner.request_raw(peer_id, method, params).await
    }

    fn connection_status_stream(&self) -> BoxStream<'static, BTreeMap<PeerId, bool>> {
        self.inner.connection_status_stream()
    }

    async fn wait_for_initialized_connections(&self) {
        self.inner.wait_for_initialized_connections().await;
    }

    async fn get_peer_connection(&self, peer_id: PeerId) -> ServerResult<DynGuaridianConnection> {
        self.inner.get_peer_connection(peer_id).await
    }
}

#[apply(async_trait_maybe_send!)]
impl<T> IGlobalFederationApi for GlobalFederationApiWithCache<T>
where
    T: IRawFederationApi + MaybeSend + MaybeSync + 'static,
{
    async fn submit_transaction(
        &self,
        tx: Transaction,
    ) -> SerdeModuleEncoding<TransactionSubmissionOutcome> {
        self.request_current_consensus_retry(
            SUBMIT_TRANSACTION_ENDPOINT.to_owned(),
            ApiRequestErased::new(SerdeTransaction::from(&tx)),
        )
        .await
    }

    async fn await_transaction(&self, txid: TransactionId) -> TransactionId {
        self.request_current_consensus_retry(
            AWAIT_TRANSACTION_ENDPOINT.to_owned(),
            ApiRequestErased::new(txid),
        )
        .await
    }

    async fn liveness(&self) -> FederationResult<()> {
        self.request_current_consensus(LIVENESS_ENDPOINT.to_owned(), ApiRequestErased::default())
            .await
    }
}
