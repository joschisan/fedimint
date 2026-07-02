use std::collections::BTreeMap;

use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::OperationId;
use fedimint_core::db::{Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{OutPoint, impl_db_lookup, impl_db_record};
use fedimint_eventlog::EventLogId;
use fedimint_lnv2_common::LightningInvoice;
use fedimint_lnv2_common::contracts::{IncomingContract, OutgoingContract};
use futures::StreamExt;

/// Database key prefixes for the gateway's metadata database, mirroring the
/// picomint gateway daemon's schema.
///
/// The gateway persists only what it cannot reconstruct from the wire: the
/// single root entropy, the set of joined federations (their `ClientConfig`),
/// the in-flight LNv2 contracts, the LDK-event idempotency set and the
/// per-federation event-log trailer cursor, and — behind
/// [`DbKeyPrefix::ClientDatabase`] — the namespaced per-federation client
/// databases. Everything else (the gateway identity keypair, the
/// per-federation client secrets) is derived from the root entropy.
///
/// There are no migrations: `gatewaydv2` is a fresh, cloud-only binary, so the
/// schema starts unversioned.
#[repr(u8)]
#[derive(Clone, Debug)]
enum DbKeyPrefix {
    /// The BIP39 root entropy, written once on first boot. Drives the gateway
    /// identity keypair and every per-federation client secret.
    RootEntropy = 0x00,
    /// Prefix under which each federation's isolated client database lives.
    ClientDatabase = 0x01,
    /// `FederationId -> ClientConfig` for every joined federation.
    ClientConfig = 0x02,
    /// Set of `FederationId`s whose public-facing endpoints are gated off.
    /// "Leaving" a federation only disables it; the config and client state
    /// are retained so in-flight payments settle and it can be re-enabled.
    DisabledFederation = 0x03,
    /// `OperationId -> OutgoingContractRow` for outgoing (send) contracts the
    /// gateway is paying. Keyed by `OperationId::from_encodable(payment_hash)`.
    /// Looked up by the LDK `PaymentSuccessful`/`PaymentFailed` handlers to
    /// finalize external sends, and by the receive trailer to finalize direct
    /// swaps.
    OutgoingContract = 0x04,
    /// `OperationId -> IncomingContractRow` for registered incoming (receive)
    /// contracts. Keyed by `OperationId::from_encodable(payment_hash)`. Looked
    /// up by the LDK `PaymentClaimable` handler and by `verify`.
    IncomingContract = 0x05,
    /// Set of LDK-event `payment_hash`es already processed by the event loop.
    /// Written atomically with the handler's work, so presence implies the
    /// handler ran to completion and it is safe to skip re-processing.
    ProcessedLdkEvent = 0x06,
    /// `FederationId -> EventLogId`: the cursor for that federation's receive
    /// trailer. Advanced past each dispatched event; a crashed trailer just
    /// re-dispatches idempotently from the persisted cursor on restart.
    TrailerCursor = 0x07,
}

/// Returns the isolated, prefix-namespaced database for a federation's client.
pub fn get_client_database(db: &Database, federation_id: &FederationId) -> Database {
    let mut prefix = vec![DbKeyPrefix::ClientDatabase as u8];
    prefix.append(&mut federation_id.consensus_encode_to_vec());
    db.with_prefix(prefix)
}

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct RootEntropyKey;

impl_db_record!(
    key = RootEntropyKey,
    value = Vec<u8>,
    db_prefix = DbKeyPrefix::RootEntropy,
);

#[derive(Clone, Debug, Encodable, Decodable, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct GatewayClientConfigKey {
    pub federation_id: FederationId,
}

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct GatewayClientConfigKeyPrefix;

impl_db_record!(
    key = GatewayClientConfigKey,
    value = ClientConfig,
    db_prefix = DbKeyPrefix::ClientConfig,
);

impl_db_lookup!(
    key = GatewayClientConfigKey,
    query_prefix = GatewayClientConfigKeyPrefix
);

#[derive(Clone, Debug, Encodable, Decodable, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct DisabledFederationKey {
    pub federation_id: FederationId,
}

impl_db_record!(
    key = DisabledFederationKey,
    value = (),
    db_prefix = DbKeyPrefix::DisabledFederation,
);

/// Row describing an outgoing (send) contract the gateway is paying.
#[derive(Debug, Clone, Encodable, Decodable)]
pub struct OutgoingContractRow {
    pub federation_id: FederationId,
    pub contract: OutgoingContract,
    pub outpoint: OutPoint,
    pub invoice: LightningInvoice,
}

#[derive(Debug, Encodable, Decodable)]
pub struct OutgoingContractKey(pub OperationId);

impl_db_record!(
    key = OutgoingContractKey,
    value = OutgoingContractRow,
    db_prefix = DbKeyPrefix::OutgoingContract,
);

/// Row describing a registered incoming (receive) contract and the invoice the
/// gateway issued for it.
#[derive(Debug, Clone, Encodable, Decodable)]
pub struct IncomingContractRow {
    pub federation_id: FederationId,
    pub contract: IncomingContract,
    pub invoice: LightningInvoice,
}

#[derive(Debug, Encodable, Decodable)]
pub struct IncomingContractKey(pub OperationId);

impl_db_record!(
    key = IncomingContractKey,
    value = IncomingContractRow,
    db_prefix = DbKeyPrefix::IncomingContract,
);

#[derive(Debug, Encodable, Decodable)]
pub struct ProcessedLdkEventKey(pub [u8; 32]);

impl_db_record!(
    key = ProcessedLdkEventKey,
    value = (),
    db_prefix = DbKeyPrefix::ProcessedLdkEvent,
);

#[derive(Debug, Encodable, Decodable)]
pub struct TrailerCursorKey(pub FederationId);

impl_db_record!(
    key = TrailerCursorKey,
    value = EventLogId,
    db_prefix = DbKeyPrefix::TrailerCursor,
);

#[allow(async_fn_in_trait)]
pub trait GatewayDbtxNcExt {
    /// Persists the BIP39 root entropy. Written once on first boot.
    async fn save_root_entropy(&mut self, entropy: &[u8]);

    /// Reads the BIP39 root entropy, if it has been established.
    async fn load_root_entropy(&mut self) -> Option<Vec<u8>>;

    /// Persists the `ClientConfig` for a joined federation, returning the
    /// previous config if one was already stored.
    async fn save_client_config(
        &mut self,
        federation_id: &FederationId,
        config: &ClientConfig,
    ) -> Option<ClientConfig>;

    async fn load_client_config(&mut self, federation_id: FederationId) -> Option<ClientConfig>;

    async fn load_client_configs(&mut self) -> BTreeMap<FederationId, ClientConfig>;

    /// Disables a federation, gating off its public-facing endpoints.
    async fn save_disabled_federation(&mut self, federation_id: FederationId);

    /// Re-enables a previously disabled federation.
    async fn remove_disabled_federation(&mut self, federation_id: FederationId);

    /// Returns whether a federation's public-facing endpoints are gated off.
    async fn is_federation_disabled(&mut self, federation_id: FederationId) -> bool;

    /// Saves an outgoing contract row, returning the previous row for the same
    /// operation if it existed.
    async fn save_outgoing_contract(
        &mut self,
        operation_id: OperationId,
        row: OutgoingContractRow,
    ) -> Option<OutgoingContractRow>;

    async fn load_outgoing_contract(
        &mut self,
        operation_id: OperationId,
    ) -> Option<OutgoingContractRow>;

    /// Saves a registered incoming contract row, returning the previous row for
    /// the same operation if it existed.
    async fn save_incoming_contract(
        &mut self,
        operation_id: OperationId,
        row: IncomingContractRow,
    ) -> Option<IncomingContractRow>;

    async fn load_incoming_contract(
        &mut self,
        operation_id: OperationId,
    ) -> Option<IncomingContractRow>;

    /// Marks an LDK event `payment_hash` as processed. Returns `true` if it was
    /// already marked (i.e. the caller should skip re-processing).
    async fn mark_ldk_event_processed(&mut self, payment_hash: [u8; 32]) -> bool;

    /// Returns whether an LDK event `payment_hash` was processed, i.e. whether
    /// an upstream Lightning HTLC actually arrived for it (distinguishing an
    /// external LN receive from an internal direct swap in the trailer).
    async fn is_ldk_event_processed(&mut self, payment_hash: [u8; 32]) -> bool;

    /// Reads the receive-trailer cursor for a federation, if one is stored.
    async fn load_trailer_cursor(&mut self, federation_id: FederationId) -> Option<EventLogId>;

    /// Persists the receive-trailer cursor for a federation.
    async fn save_trailer_cursor(&mut self, federation_id: FederationId, cursor: EventLogId);
}

impl<Cap: Send> GatewayDbtxNcExt for DatabaseTransaction<'_, Cap> {
    async fn save_root_entropy(&mut self, entropy: &[u8]) {
        self.insert_entry(&RootEntropyKey, &entropy.to_vec()).await;
    }

    async fn load_root_entropy(&mut self) -> Option<Vec<u8>> {
        self.get_value(&RootEntropyKey).await
    }

    async fn save_client_config(
        &mut self,
        federation_id: &FederationId,
        config: &ClientConfig,
    ) -> Option<ClientConfig> {
        self.insert_entry(
            &GatewayClientConfigKey {
                federation_id: *federation_id,
            },
            config,
        )
        .await
    }

    async fn load_client_config(&mut self, federation_id: FederationId) -> Option<ClientConfig> {
        self.get_value(&GatewayClientConfigKey { federation_id })
            .await
    }

    async fn load_client_configs(&mut self) -> BTreeMap<FederationId, ClientConfig> {
        self.find_by_prefix(&GatewayClientConfigKeyPrefix)
            .await
            .map(|(key, config): (GatewayClientConfigKey, ClientConfig)| {
                (key.federation_id, config)
            })
            .collect::<BTreeMap<FederationId, ClientConfig>>()
            .await
    }

    async fn save_disabled_federation(&mut self, federation_id: FederationId) {
        self.insert_entry(&DisabledFederationKey { federation_id }, &())
            .await;
    }

    async fn remove_disabled_federation(&mut self, federation_id: FederationId) {
        self.remove_entry(&DisabledFederationKey { federation_id })
            .await;
    }

    async fn is_federation_disabled(&mut self, federation_id: FederationId) -> bool {
        self.get_value(&DisabledFederationKey { federation_id })
            .await
            .is_some()
    }

    async fn save_outgoing_contract(
        &mut self,
        operation_id: OperationId,
        row: OutgoingContractRow,
    ) -> Option<OutgoingContractRow> {
        self.insert_entry(&OutgoingContractKey(operation_id), &row)
            .await
    }

    async fn load_outgoing_contract(
        &mut self,
        operation_id: OperationId,
    ) -> Option<OutgoingContractRow> {
        self.get_value(&OutgoingContractKey(operation_id)).await
    }

    async fn save_incoming_contract(
        &mut self,
        operation_id: OperationId,
        row: IncomingContractRow,
    ) -> Option<IncomingContractRow> {
        self.insert_entry(&IncomingContractKey(operation_id), &row)
            .await
    }

    async fn load_incoming_contract(
        &mut self,
        operation_id: OperationId,
    ) -> Option<IncomingContractRow> {
        self.get_value(&IncomingContractKey(operation_id)).await
    }

    async fn mark_ldk_event_processed(&mut self, payment_hash: [u8; 32]) -> bool {
        self.insert_entry(&ProcessedLdkEventKey(payment_hash), &())
            .await
            .is_some()
    }

    async fn is_ldk_event_processed(&mut self, payment_hash: [u8; 32]) -> bool {
        self.get_value(&ProcessedLdkEventKey(payment_hash))
            .await
            .is_some()
    }

    async fn load_trailer_cursor(&mut self, federation_id: FederationId) -> Option<EventLogId> {
        self.get_value(&TrailerCursorKey(federation_id)).await
    }

    async fn save_trailer_cursor(&mut self, federation_id: FederationId, cursor: EventLogId) {
        self.insert_entry(&TrailerCursorKey(federation_id), &cursor)
            .await;
    }
}
