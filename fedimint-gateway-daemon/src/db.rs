use std::collections::BTreeMap;

use bitcoin::hashes::{Hash, sha256};
use fedimint_core::config::FederationId;
use fedimint_core::db::{Database, DatabaseTransaction, IDatabaseTransactionOpsCoreTyped};
use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::{Amount, impl_db_lookup, impl_db_record, push_db_pair_items, secp256k1};
use fedimint_gateway_common::{FederationConfig, RegisteredProtocol};
use fedimint_lnv2_common::contracts::{IncomingContract, PaymentImage};
use futures::StreamExt;
use rand::rngs::OsRng;
use secp256k1::{Keypair, Secp256k1};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

pub trait GatewayDbExt {
    fn get_client_database(&self, federation_id: &FederationId) -> Database;
}

impl GatewayDbExt for Database {
    fn get_client_database(&self, federation_id: &FederationId) -> Database {
        let mut prefix = vec![DbKeyPrefix::ClientDatabase as u8];
        prefix.append(&mut federation_id.consensus_encode_to_vec());
        self.with_prefix(prefix)
    }
}

#[allow(async_fn_in_trait)]
pub trait GatewayDbtxNcExt {
    async fn save_federation_config(&mut self, config: &FederationConfig);
    async fn load_federation_configs(&mut self) -> BTreeMap<FederationId, FederationConfig>;
    async fn load_federation_config(
        &mut self,
        federation_id: FederationId,
    ) -> Option<FederationConfig>;
    async fn remove_federation_config(&mut self, federation_id: FederationId);

    /// Returns the keypair that uniquely identifies the gateway, creating it if
    /// it does not exist. Remember to commit the transaction after calling this
    /// method.
    async fn load_or_create_gateway_keypair(&mut self, protocol: RegisteredProtocol) -> Keypair;

    async fn save_new_preimage_authentication(
        &mut self,
        payment_hash: sha256::Hash,
        preimage_auth: sha256::Hash,
    );

    async fn load_preimage_authentication(
        &mut self,
        payment_hash: sha256::Hash,
    ) -> Option<sha256::Hash>;

    /// Saves a registered incoming contract, returning the previous contract
    /// with the same payment hash if it existed.
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

    /// Reads and serializes structures from the gateway's database for the
    /// purpose for serializing to JSON for inspection.
    async fn dump_database(
        &mut self,
        prefix_names: Vec<String>,
    ) -> BTreeMap<String, Box<dyn erased_serde::Serialize + Send>>;
}

impl<Cap: Send> GatewayDbtxNcExt for DatabaseTransaction<'_, Cap> {
    async fn save_federation_config(&mut self, config: &FederationConfig) {
        let id = config.invite_code.federation_id();
        self.insert_entry(&FederationConfigKey { id }, config).await;
    }

    async fn load_federation_configs(&mut self) -> BTreeMap<FederationId, FederationConfig> {
        self.find_by_prefix(&FederationConfigKeyPrefix)
            .await
            .map(|(key, config): (FederationConfigKey, FederationConfig)| (key.id, config))
            .collect::<BTreeMap<FederationId, FederationConfig>>()
            .await
    }

    async fn load_federation_config(
        &mut self,
        federation_id: FederationId,
    ) -> Option<FederationConfig> {
        self.get_value(&FederationConfigKey { id: federation_id })
            .await
    }

    async fn remove_federation_config(&mut self, federation_id: FederationId) {
        self.remove_entry(&FederationConfigKey { id: federation_id })
            .await;
    }

    async fn load_or_create_gateway_keypair(&mut self, protocol: RegisteredProtocol) -> Keypair {
        if let Some(key_pair) = self
            .get_value(&GatewayPublicKey {
                protocol: protocol.clone(),
            })
            .await
        {
            key_pair
        } else {
            let context = Secp256k1::new();
            let (secret_key, _public_key) = context.generate_keypair(&mut OsRng);
            let key_pair = Keypair::from_secret_key(&context, &secret_key);

            self.insert_new_entry(&GatewayPublicKey { protocol }, &key_pair)
                .await;
            key_pair
        }
    }

    async fn save_new_preimage_authentication(
        &mut self,
        payment_hash: sha256::Hash,
        preimage_auth: sha256::Hash,
    ) {
        self.insert_new_entry(&PreimageAuthentication { payment_hash }, &preimage_auth)
            .await;
    }

    async fn load_preimage_authentication(
        &mut self,
        payment_hash: sha256::Hash,
    ) -> Option<sha256::Hash> {
        self.get_value(&PreimageAuthentication { payment_hash })
            .await
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

    async fn dump_database(
        &mut self,
        prefix_names: Vec<String>,
    ) -> BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> {
        let mut gateway_items: BTreeMap<String, Box<dyn erased_serde::Serialize + Send>> =
            BTreeMap::new();
        let filtered_prefixes = DbKeyPrefix::iter().filter(|f| {
            prefix_names.is_empty() || prefix_names.contains(&f.to_string().to_lowercase())
        });

        for table in filtered_prefixes {
            match table {
                DbKeyPrefix::FederationConfig => {
                    push_db_pair_items!(
                        self,
                        FederationConfigKeyPrefix,
                        FederationConfigKey,
                        FederationConfig,
                        gateway_items,
                        "Federation Config"
                    );
                }
                DbKeyPrefix::GatewayPublicKey => {
                    push_db_pair_items!(
                        self,
                        GatewayPublicKeyPrefix,
                        GatewayPublicKey,
                        Keypair,
                        gateway_items,
                        "Gateway Public Keys"
                    );
                }
                _ => {}
            }
        }

        gateway_items
    }
}

#[repr(u8)]
#[derive(Clone, EnumIter, Debug)]
enum DbKeyPrefix {
    FederationConfig = 0x04,
    GatewayPublicKey = 0x06,
    PreimageAuthentication = 0x08,
    RegisteredIncomingContract = 0x09,
    ClientDatabase = 0x10,
}

impl std::fmt::Display for DbKeyPrefix {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}

#[derive(Debug, Encodable, Decodable)]
struct FederationConfigKeyPrefix;

#[derive(Debug, Clone, Encodable, Decodable, Eq, PartialEq, Hash, Ord, PartialOrd)]
struct FederationConfigKey {
    id: FederationId,
}

impl_db_record!(
    key = FederationConfigKey,
    value = FederationConfig,
    db_prefix = DbKeyPrefix::FederationConfig,
);

impl_db_lookup!(
    key = FederationConfigKey,
    query_prefix = FederationConfigKeyPrefix
);

#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
struct GatewayPublicKey {
    protocol: RegisteredProtocol,
}

#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
struct GatewayPublicKeyPrefix;

impl_db_record!(
    key = GatewayPublicKey,
    value = Keypair,
    db_prefix = DbKeyPrefix::GatewayPublicKey,
);

impl_db_lookup!(
    key = GatewayPublicKey,
    query_prefix = GatewayPublicKeyPrefix
);

#[derive(Debug, Clone, Eq, PartialEq, Encodable, Decodable)]
struct PreimageAuthentication {
    payment_hash: sha256::Hash,
}

impl_db_record!(
    key = PreimageAuthentication,
    value = sha256::Hash,
    db_prefix = DbKeyPrefix::PreimageAuthentication
);

#[derive(Debug, Encodable, Decodable)]
struct RegisteredIncomingContractKey(pub PaymentImage);

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
