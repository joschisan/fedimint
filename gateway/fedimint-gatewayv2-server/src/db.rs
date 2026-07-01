use std::collections::BTreeMap;

use fedimint_core::config::{ClientConfig, FederationId};
use fedimint_core::db::{Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{Amount, impl_db_lookup, impl_db_record};
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use futures::StreamExt;

/// Database key prefixes for the gateway's metadata database.
///
/// The gateway persists only what it cannot reconstruct from the wire: the
/// single root entropy, the set of joined federations (their `ClientConfig`),
/// the registered LNv2 incoming contracts, and — behind
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
    /// `PaymentImage -> RegisteredIncomingContract` for registered LNv2
    /// incoming contracts.
    RegisteredIncomingContract = 0x04,
    /// Work-list of incoming payments whose upstream Lightning HTLC has become
    /// claimable (an LDK `PaymentClaimable` fired) but which the gateway has
    /// not yet finished settling. Seeds the per-client claim trailer on
    /// boot; an entry is removed once the receive reaches a terminal state.
    /// Bounding the boot scan to genuinely-pending payments is why this
    /// exists separately from [`DbKeyPrefix::RegisteredIncomingContract`]
    /// (which is retained for the lifetime of the payment so `verify` can
    /// read settled receives).
    PendingIncomingClaim = 0x05,
    /// Set of payment images whose upstream HTLC the gateway has already
    /// claimed. Gates double-claims when a live claim task and the boot
    /// reconciliation race on the same payment.
    ClaimedIncomingContract = 0x06,
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

#[derive(Debug, Encodable, Decodable)]
pub struct RegisteredIncomingContractKey(pub PaymentImage);

#[derive(Debug, Encodable, Decodable)]
pub struct RegisteredIncomingContract {
    pub federation_id: FederationId,
    /// The amount of the incoming contract, in msats.
    pub incoming_amount_msats: u64,
    pub contract: IncomingContract,
}

impl_db_record!(
    key = RegisteredIncomingContractKey,
    value = RegisteredIncomingContract,
    db_prefix = DbKeyPrefix::RegisteredIncomingContract,
);

#[derive(Debug, Encodable, Decodable)]
pub struct PendingIncomingClaimKey(pub PaymentImage);

#[derive(Debug, Encodable, Decodable)]
pub struct PendingIncomingClaimKeyPrefix;

impl_db_record!(
    key = PendingIncomingClaimKey,
    value = (),
    db_prefix = DbKeyPrefix::PendingIncomingClaim,
);

impl_db_lookup!(
    key = PendingIncomingClaimKey,
    query_prefix = PendingIncomingClaimKeyPrefix
);

#[derive(Debug, Encodable, Decodable)]
pub struct ClaimedIncomingContractKey(pub PaymentImage);

impl_db_record!(
    key = ClaimedIncomingContractKey,
    value = (),
    db_prefix = DbKeyPrefix::ClaimedIncomingContract,
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

    /// Saves a registered incoming contract, returning the previous contract
    /// with the same payment image if it existed.
    async fn save_registered_incoming_contract(
        &mut self,
        federation_id: FederationId,
        incoming_amount: Amount,
        contract: IncomingContract,
    ) -> Option<RegisteredIncomingContract>;

    async fn load_registered_incoming_contract(
        &mut self,
        payment_image: PaymentImage,
    ) -> Option<RegisteredIncomingContract>;

    /// Records that an incoming payment's upstream HTLC has become claimable
    /// and still needs settling. Idempotent.
    async fn save_pending_claim(&mut self, payment_image: PaymentImage);

    /// Removes an incoming payment from the pending-claim work-list once it has
    /// reached a terminal state (claimed or refunded).
    async fn remove_pending_claim(&mut self, payment_image: PaymentImage);

    /// Returns every incoming payment still awaiting settlement, used to seed
    /// the claim trailer on boot.
    async fn load_pending_claims(&mut self) -> Vec<PaymentImage>;

    /// Marks an incoming payment's upstream HTLC as claimed, gating double
    /// claims. Returns whether it was already marked.
    async fn save_claimed_incoming_contract(&mut self, payment_image: PaymentImage) -> bool;

    async fn is_incoming_contract_claimed(&mut self, payment_image: PaymentImage) -> bool;
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

    async fn save_registered_incoming_contract(
        &mut self,
        federation_id: FederationId,
        incoming_amount: Amount,
        contract: IncomingContract,
    ) -> Option<RegisteredIncomingContract> {
        self.insert_entry(
            &RegisteredIncomingContractKey(contract.commitment.payment_image.clone()),
            &RegisteredIncomingContract {
                federation_id,
                incoming_amount_msats: incoming_amount.msats,
                contract,
            },
        )
        .await
    }

    async fn load_registered_incoming_contract(
        &mut self,
        payment_image: PaymentImage,
    ) -> Option<RegisteredIncomingContract> {
        self.get_value(&RegisteredIncomingContractKey(payment_image))
            .await
    }

    async fn save_pending_claim(&mut self, payment_image: PaymentImage) {
        self.insert_entry(&PendingIncomingClaimKey(payment_image), &())
            .await;
    }

    async fn remove_pending_claim(&mut self, payment_image: PaymentImage) {
        self.remove_entry(&PendingIncomingClaimKey(payment_image))
            .await;
    }

    async fn load_pending_claims(&mut self) -> Vec<PaymentImage> {
        self.find_by_prefix(&PendingIncomingClaimKeyPrefix)
            .await
            .map(|(key, ()): (PendingIncomingClaimKey, ())| key.0)
            .collect::<Vec<PaymentImage>>()
            .await
    }

    async fn save_claimed_incoming_contract(&mut self, payment_image: PaymentImage) -> bool {
        self.insert_entry(&ClaimedIncomingContractKey(payment_image), &())
            .await
            .is_some()
    }

    async fn is_incoming_contract_claimed(&mut self, payment_image: PaymentImage) -> bool {
        self.get_value(&ClaimedIncomingContractKey(payment_image))
            .await
            .is_some()
    }
}
