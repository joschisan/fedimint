use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::core::OperationId;
use fedimint_core::db::Database;
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{OutPoint, impl_db_lookup, impl_db_record};
use fedimint_eventlog::EventLogId;
use fedimint_lnv2_common::LightningInvoice;
use fedimint_lnv2_common::contracts::{IncomingContract, OutgoingContract};

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

/// The BIP39 root entropy, written once on first boot.
#[derive(Clone, Debug, Encodable, Decodable)]
pub struct RootEntropyKey;

impl_db_record!(
    key = RootEntropyKey,
    value = Vec<u8>,
    db_prefix = DbKeyPrefix::RootEntropy,
);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct ClientConfigKey(pub FederationId);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct ClientConfigKeyPrefix;

impl_db_record!(
    key = ClientConfigKey,
    value = ClientConfig,
    db_prefix = DbKeyPrefix::ClientConfig,
);

impl_db_lookup!(key = ClientConfigKey, query_prefix = ClientConfigKeyPrefix);

#[derive(Clone, Debug, Encodable, Decodable)]
pub struct DisabledFederationKey(pub FederationId);

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
