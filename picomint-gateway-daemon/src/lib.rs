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
#![allow(clippy::unused_async)]

pub mod cli;
pub mod client;
pub mod db;
pub mod public;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, anyhow};
use async_trait::async_trait;
use bitcoin::Network;
use bitcoin::hashes::{Hash, sha256};
use client::GatewayClientFactory;
use ldk_node::payment::{PaymentKind, PaymentStatus};
use lightning::ln::channelmanager::PaymentId;
use lightning::routing::router::RouteParametersConfig;
use lightning::types::payment::{PaymentHash, PaymentPreimage};
use lightning_invoice::{
    Bolt11Invoice, Bolt11InvoiceDescription as LdkBolt11InvoiceDescription, Description,
};
use picomint_client::Client;
use picomint_client::gw::{
    EXPIRATION_DELTA_MINIMUM, FinalReceiveState, IGatewayClient, LightningRpcError, PaymentAction,
};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::invite_code::InviteCode;
use picomint_core::ln::Bolt11InvoiceDescription;
use picomint_core::ln::contracts::{IncomingContract, PaymentImage};
use picomint_core::ln::gateway_api::{
    CreateBolt11InvoicePayload, PaymentFee, RoutingInfo, SendPaymentPayload,
};
use picomint_core::secp256k1::PublicKey;
use picomint_core::secp256k1::schnorr::Signature;
use picomint_core::time::duration_since_epoch;
use picomint_core::{Amount, PeerId};
use picomint_gateway_cli_core::FederationInfo;
use picomint_lnurl::VerifyResponse;
use picomint_logging::LOG_GATEWAY;
use picomint_redb::Database;
use tokio::sync::RwLock;
use tracing::{error, warn};

use crate::db::{
    REGISTERED_INCOMING_CONTRACT, RegisteredIncomingContract as DbRegisteredIncomingContract,
};

/// Default Bitcoin network for testing purposes.
pub const DEFAULT_NETWORK: Network = Network::Regtest;

/// Name of the gateway's database.
pub const DB_FILE: &str = "database.redb";

/// Name of the folder for LDK node data.
pub const LDK_NODE_DB_FOLDER: &str = "ldk_node";

#[derive(Clone)]
pub struct AppState {
    pub clients: Arc<RwLock<BTreeMap<FederationId, Arc<Client>>>>,
    pub node: Arc<ldk_node::Node>,
    pub client_factory: GatewayClientFactory,
    pub gateway_db: Database,
    pub api_addr: SocketAddr,
    pub data_dir: std::path::PathBuf,
    pub network: Network,
    pub routing_fees: PaymentFee,
    pub transaction_fees: PaymentFee,
    pub outbound_lightning_payment_lock_pool: Arc<lockable::LockPool<PaymentId>>,
}

impl std::fmt::Debug for AppState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AppState")
            .field("gateway_db", &self.gateway_db)
            .field("api_addr", &self.api_addr)
            .field("data_dir", &self.data_dir)
            .field("node_id", &self.node.node_id())
            .finish_non_exhaustive()
    }
}

impl AppState {
    /// Retrieves a client for a given federation.
    pub async fn select_client(&self, federation_id: FederationId) -> Option<Arc<Client>> {
        self.clients.read().await.get(&federation_id).cloned()
    }

    /// Load all persisted federation clients on startup.
    pub async fn load_clients(&self) -> anyhow::Result<()> {
        let mut clients = self.clients.write().await;

        let federations = self.client_factory.list_federations().await;

        for federation_id in federations {
            let gateway = Arc::new(self.clone());
            match self.client_factory.load(&federation_id, gateway).await {
                Ok(Some(client)) => {
                    clients.insert(client.federation_id(), client);
                }
                Ok(None) => {
                    warn!(target: LOG_GATEWAY, %federation_id, "Client DB not initialized, skipping");
                }
                Err(err) => {
                    warn!(target: LOG_GATEWAY, %federation_id, %err, "Failed to load client");
                }
            }
        }

        Ok(())
    }

    /// Verifies that the federation's lightning module network matches the
    /// gateway's network.
    pub async fn check_federation_network(
        client: &Arc<Client>,
        network: Network,
    ) -> anyhow::Result<()> {
        let federation_id = client.federation_id();
        let config = client.config().await;

        if config.ln.network != network {
            error!(
                target: LOG_GATEWAY,
                %federation_id,
                %network,
                "Incorrect network for federation",
            );
            return Err(anyhow::anyhow!(format!(
                "Unsupported network {}",
                config.ln.network
            )));
        }

        Ok(())
    }

    /// Get the name of a federation from its client config.
    pub async fn federation_name(client: &Arc<Client>) -> Option<String> {
        client.config().await.federation_name()
    }

    /// Get info for all connected federations.
    pub async fn federation_info_all(&self) -> Vec<FederationInfo> {
        let clients = self.clients.read().await;
        let mut infos = Vec::new();
        for (federation_id, client) in clients.iter() {
            infos.push(FederationInfo {
                federation_id: *federation_id,
                federation_name: Self::federation_name(client).await,
            });
        }
        infos
    }

    /// Get JSON client configs for all connected federations.
    pub async fn all_federation_configs(&self) -> BTreeMap<FederationId, serde_json::Value> {
        let clients = self.clients.read().await;
        let mut configs = BTreeMap::new();
        for (federation_id, client) in clients.iter() {
            let config = client.config().await;
            configs.insert(
                *federation_id,
                serde_json::to_value(&config).expect("ConsensusConfig is serializable"),
            );
        }
        configs
    }

    /// Get invite codes for all connected federations.
    pub async fn all_invite_codes(
        &self,
    ) -> BTreeMap<FederationId, BTreeMap<PeerId, (String, InviteCode)>> {
        let clients = self.clients.read().await;
        let mut invite_codes = BTreeMap::new();
        for (federation_id, client) in clients.iter() {
            let config = client.config().await;
            let mut fed_codes = BTreeMap::new();
            for (peer_id, endpoints) in &config.iroh_endpoints {
                if let Some(code) = client.invite_code(*peer_id).await {
                    fed_codes.insert(*peer_id, (endpoints.name.clone(), code));
                }
            }
            invite_codes.insert(*federation_id, fed_codes);
        }
        invite_codes
    }
}

// Lightning Gateway implementation
impl AppState {
    async fn public_key(&self, federation_id: &FederationId) -> Option<PublicKey> {
        self.clients
            .read()
            .await
            .get(federation_id)
            .map(|client| client.gw().keypair.public_key())
    }

    pub async fn routing_info(
        &self,
        federation_id: &FederationId,
    ) -> anyhow::Result<Option<RoutingInfo>> {
        self.select_client(*federation_id)
            .await
            .context("Federation not connected")?;

        Ok(self
            .public_key(federation_id)
            .await
            .map(|module_public_key| RoutingInfo {
                lightning_public_key: self.node.node_id(),
                module_public_key,
                send_fee_default: self.routing_fees + self.transaction_fees,
                send_fee_minimum: self.transaction_fees,
                expiration_delta_default: 1440,
                expiration_delta_minimum: EXPIRATION_DELTA_MINIMUM,
                receive_fee: self.transaction_fees,
            }))
    }

    pub async fn send_payment(
        &self,
        payload: SendPaymentPayload,
    ) -> anyhow::Result<std::result::Result<[u8; 32], Signature>> {
        self.select_client(payload.federation_id)
            .await
            .context("Federation not connected")?
            .gw()
            .send_payment(payload)
            .await
            .map_err(|e| anyhow::anyhow!(format!("Outgoing payment error: {e}")))
    }

    pub async fn create_bolt11_invoice(
        &self,
        payload: CreateBolt11InvoicePayload,
    ) -> anyhow::Result<Bolt11Invoice> {
        if !payload.contract.verify() {
            return Err(anyhow::anyhow!(
                "Incoming payment error: The contract is invalid",
            ));
        }

        let payment_info = self
            .routing_info(&payload.federation_id)
            .await?
            .with_context(|| {
                format!(
                    "Incoming payment error: Federation {} does not exist",
                    payload.federation_id
                )
            })?;

        if payload.contract.commitment.refund_pk != payment_info.module_public_key {
            return Err(anyhow::anyhow!(
                "Incoming payment error: The incoming contract is keyed to another gateway",
            ));
        }

        let contract_amount = payment_info.receive_fee.subtract_from(payload.amount.msats);

        if contract_amount == Amount::ZERO {
            return Err(anyhow::anyhow!(
                "Incoming payment error: Zero amount incoming contracts are not supported",
            ));
        }

        if contract_amount != payload.contract.commitment.amount {
            return Err(anyhow::anyhow!(
                "Incoming payment error: The contract amount does not pay the correct amount of fees",
            ));
        }

        if payload.contract.commitment.expiration <= duration_since_epoch().as_secs() {
            return Err(anyhow::anyhow!(
                "Incoming payment error: The contract has already expired",
            ));
        }

        let payment_hash = match payload.contract.commitment.payment_image {
            PaymentImage::Hash(payment_hash) => payment_hash,
            PaymentImage::Point(..) => {
                return Err(anyhow::anyhow!(
                    "Incoming payment error: PaymentImage is not a payment hash",
                ));
            }
        };

        let invoice = self
            .create_invoice_via_lnrpc(
                payment_hash,
                payload.amount,
                payload.description.clone(),
                payload.expiry_secs,
            )
            .await?;

        let dbtx = self.gateway_db.begin_write().await;

        if dbtx
            .as_ref()
            .insert(
                &REGISTERED_INCOMING_CONTRACT,
                &payload.contract.commitment.payment_image.clone(),
                &DbRegisteredIncomingContract {
                    federation_id: payload.federation_id,
                    incoming_amount_msats: payload.amount.msats,
                    contract: payload.contract,
                },
            )
            .is_some()
        {
            return Err(anyhow::anyhow!(
                "Incoming payment error: PaymentHash is already registered",
            ));
        }

        dbtx.commit().await;

        Ok(invoice)
    }

    pub async fn create_invoice_via_lnrpc(
        &self,
        payment_hash: sha256::Hash,
        amount: Amount,
        description: Bolt11InvoiceDescription,
        expiry_time: u32,
    ) -> std::result::Result<Bolt11Invoice, LightningRpcError> {
        let ph = PaymentHash(*payment_hash.as_byte_array());

        let ldk_description = match description {
            Bolt11InvoiceDescription::Direct(desc) => {
                LdkBolt11InvoiceDescription::Direct(Description::new(desc).map_err(|_| {
                    LightningRpcError::FailedToGetInvoice {
                        failure_reason: "Invalid description".to_string(),
                    }
                })?)
            }
            Bolt11InvoiceDescription::Hash(hash) => {
                LdkBolt11InvoiceDescription::Hash(lightning_invoice::Sha256(hash))
            }
        };

        self.node
            .bolt11_payment()
            .receive_for_hash(amount.msats, &ldk_description, expiry_time, ph)
            .map_err(|e| LightningRpcError::FailedToGetInvoice {
                failure_reason: e.to_string(),
            })
    }

    pub async fn verify_bolt11_preimage(
        &self,
        payment_hash: sha256::Hash,
        wait: bool,
    ) -> std::result::Result<VerifyResponse, String> {
        let registered_contract = self
            .gateway_db
            .begin_read()
            .await
            .as_ref()
            .get(
                &REGISTERED_INCOMING_CONTRACT,
                &PaymentImage::Hash(payment_hash),
            )
            .ok_or("Unknown payment hash".to_string())?;

        let client = self
            .select_client(registered_contract.federation_id)
            .await
            .ok_or("Not connected to federation".to_string())?;

        let operation_id = OperationId::from_encodable(&registered_contract.contract);

        // Non-wait: report the current settled state, if any, without blocking.
        // A short timeout is enough because `await_receive` resolves
        // immediately when the settle event is already in the event log.
        let state = if wait {
            client.gw().await_receive(operation_id).await
        } else {
            match tokio::time::timeout(
                Duration::from_millis(50),
                client.gw().await_receive(operation_id),
            )
            .await
            {
                Ok(state) => state,
                Err(_) => {
                    return Ok(VerifyResponse {
                        settled: false,
                        preimage: None,
                    });
                }
            }
        };

        let preimage = match state {
            FinalReceiveState::Success(preimage) => Ok(preimage),
            FinalReceiveState::Failure => Err("Payment has failed".to_string()),
            FinalReceiveState::Refunded => Err("Payment has been refunded".to_string()),
        }?;

        Ok(VerifyResponse {
            settled: true,
            preimage: Some(preimage),
        })
    }

    pub async fn get_registered_incoming_contract_and_client(
        &self,
        payment_image: PaymentImage,
        amount_msats: u64,
    ) -> anyhow::Result<(IncomingContract, Arc<Client>)> {
        let registered_incoming_contract = self
            .gateway_db
            .begin_read()
            .await
            .as_ref()
            .get(&REGISTERED_INCOMING_CONTRACT, &payment_image)
            .context("Incoming payment error: No corresponding decryption contract available")?;

        if registered_incoming_contract.incoming_amount_msats != amount_msats {
            return Err(anyhow::anyhow!(
                "Incoming payment error: The available decryption contract's amount is not equal to the requested amount",
            ));
        }

        let client = self
            .select_client(registered_incoming_contract.federation_id)
            .await
            .context("Federation not connected")?;

        Ok((registered_incoming_contract.contract, client))
    }
}

#[async_trait]
impl IGatewayClient for AppState {
    async fn complete_htlc(&self, htlc_response: picomint_client::gw::InterceptPaymentResponse) {
        let ph = PaymentHash(*htlc_response.payment_hash.as_byte_array());
        let claimable_amount_msat = 999_999_999_999_999;
        let ph_hex_str = hex::encode(htlc_response.payment_hash);

        if let PaymentAction::Settle(preimage) = htlc_response.action {
            if let Err(err) = self.node.bolt11_payment().claim_for_hash(
                ph,
                claimable_amount_msat,
                PaymentPreimage(preimage.0),
            ) {
                warn!(
                    target: LOG_GATEWAY,
                    payment_hash = %ph_hex_str,
                    err = %err,
                    "Failed to claim LDK payment",
                );
            }
        } else {
            warn!(
                target: LOG_GATEWAY,
                payment_hash = %ph_hex_str,
                "Unwinding payment because the action was not Settle",
            );
            if let Err(err) = self.node.bolt11_payment().fail_for_hash(ph) {
                warn!(
                    target: LOG_GATEWAY,
                    payment_hash = %ph_hex_str,
                    err = %err,
                    "Failed to unwind LDK payment",
                );
            }
        }
    }

    async fn try_direct_swap(
        &self,
        invoice: &Bolt11Invoice,
    ) -> anyhow::Result<Option<(FinalReceiveState, FederationId)>> {
        if self.node.node_id() != invoice.get_payee_pub_key() {
            return Ok(None);
        }

        let (contract, client) = self
            .get_registered_incoming_contract_and_client(
                PaymentImage::Hash(*invoice.payment_hash()),
                invoice
                    .amount_milli_satoshis()
                    .expect("The amount invoice has been previously checked"),
            )
            .await?;

        let federation_id = client.federation_id();
        let final_state = client
            .gw()
            .relay_direct_swap(
                contract,
                invoice
                    .amount_milli_satoshis()
                    .expect("amountless invoices are not supported"),
            )
            .await?;

        Ok(Some((final_state, federation_id)))
    }

    async fn pay(
        &self,
        invoice: Bolt11Invoice,
        max_delay: u64,
        max_fee: Amount,
    ) -> std::result::Result<[u8; 32], LightningRpcError> {
        let payment_id = PaymentId(*invoice.payment_hash().as_byte_array());

        let _payment_lock_guard = self
            .outbound_lightning_payment_lock_pool
            .async_lock(payment_id)
            .await;

        if self.node.payment(&payment_id).is_none() {
            assert_eq!(
                self.node
                    .bolt11_payment()
                    .send(
                        &invoice,
                        Some(RouteParametersConfig {
                            max_total_routing_fee_msat: Some(max_fee.msats),
                            max_total_cltv_expiry_delta: max_delay as u32,
                            ..RouteParametersConfig::default()
                        }),
                    )
                    .map_err(|e| LightningRpcError::FailedPayment {
                        failure_reason: format!("LDK payment failed to initialize: {e:?}"),
                    })?,
                payment_id
            );
        }

        loop {
            if let Some(payment_details) = self.node.payment(&payment_id) {
                match payment_details.status {
                    PaymentStatus::Pending => {}
                    PaymentStatus::Succeeded => {
                        if let PaymentKind::Bolt11 {
                            preimage: Some(preimage),
                            ..
                        } = payment_details.kind
                        {
                            return Ok(preimage.0);
                        }
                    }
                    PaymentStatus::Failed => {
                        return Err(LightningRpcError::FailedPayment {
                            failure_reason: "LDK payment failed".to_string(),
                        });
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn min_contract_amount(
        &self,
        federation_id: &FederationId,
        amount: u64,
    ) -> anyhow::Result<Amount> {
        Ok(self
            .routing_info(federation_id)
            .await?
            .ok_or(anyhow!("Routing Info not available"))?
            .send_fee_minimum
            .add_to(amount))
    }
}
