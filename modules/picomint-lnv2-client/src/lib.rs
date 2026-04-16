#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

pub use picomint_lnv2_common as common;

mod api;
mod db;
pub mod events;
mod receive_sm;
mod send_sm;

use std::sync::Arc;

use bitcoin::hashes::{Hash, sha256};
use bitcoin::secp256k1;
use db::{GATEWAY, GatewayKey, INCOMING_CONTRACT_STREAM_INDEX, SEND_OPERATION};
use lightning_invoice::{Bolt11Invoice, Currency};
use picomint_api_client::api::FederationApi;
use picomint_client_module::executor::ModuleExecutor;
use picomint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use picomint_client_module::module::{ClientContext, ClientModule};
use picomint_client_module::transaction::{ClientOutput, ClientOutputBundle, TransactionBuilder};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::encoding::Encodable;
use picomint_core::module::{ModuleCommon, ModuleInit};
use picomint_core::secp256k1::SECP256K1;
use picomint_core::task::TaskGroup;
use picomint_core::time::duration_since_epoch;
use picomint_core::util::SafeUrl;
use picomint_core::{Amount, OutPoint, PeerId, apply, async_trait_maybe_send};
use picomint_derive_secret::{ChildId, DerivableSecret};
use picomint_lnv2_common::config::LightningClientConfig;
use picomint_lnv2_common::contracts::{IncomingContract, OutgoingContract, PaymentImage};
use picomint_lnv2_common::gateway_api::{
    GatewayConnection, PaymentFee, RealGatewayConnection, RoutingInfo,
};
use picomint_lnv2_common::{
    Bolt11InvoiceDescription, GatewayApi, LightningCommonInit, LightningInvoice,
    LightningModuleTypes, LightningOutput, LightningOutputV0, MINIMUM_INCOMING_CONTRACT_AMOUNT,
    lnurl, tweak,
};
use picomint_redb::WriteTxRef;
use secp256k1::{Keypair, PublicKey, Scalar, SecretKey, ecdh};
use thiserror::Error;
use tpe::{AggregateDecryptionKey, derive_agg_dk};

use crate::api::LightningFederationApi;
use crate::events::SendPaymentEvent;
use crate::receive_sm::{ReceiveSMCommon, ReceiveSMState, ReceiveStateMachine};
use crate::send_sm::{SendSMCommon, SendSMState, SendStateMachine};

/// Number of blocks until outgoing lightning contracts times out and user
/// client can refund it unilaterally
const EXPIRATION_DELTA_LIMIT: u64 = 1440;

/// A two hour buffer in case either the client or gateway go offline
const CONTRACT_CONFIRMATION_BUFFER: u64 = 12;

pub type SendResult = Result<OperationId, SendPaymentError>;

pub type ReceiveResult = Result<(Bolt11Invoice, OperationId), ReceiveError>;

#[derive(Clone, Default)]
pub struct LightningClientInit {
    pub gateway_conn: Option<Arc<dyn GatewayConnection + Send + Sync>>,
}

impl std::fmt::Debug for LightningClientInit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LightningClientInit")
            .field("gateway_conn", &self.gateway_conn)
            .finish()
    }
}

impl ModuleInit for LightningClientInit {
    type Common = LightningCommonInit;
}

#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for LightningClientInit {
    type Module = LightningClientModule;

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        let gateway_conn = if let Some(gateway_conn) = self.gateway_conn.clone() {
            gateway_conn
        } else {
            let api = GatewayApi::new(None);
            Arc::new(RealGatewayConnection { api })
        };
        Ok(LightningClientModule::new(
            *args.federation_id(),
            args.cfg().clone(),
            args.context(),
            args.module_api().clone(),
            args.module_root_secret(),
            gateway_conn,
            args.task_group(),
        ))
    }
}

#[derive(Debug, Clone)]
pub struct LightningClientContext {
    federation_id: FederationId,
    gateway_conn: Arc<dyn GatewayConnection + Send + Sync>,
    pub(crate) client_ctx: ClientContext<LightningClientModule>,
}

#[derive(Debug, Clone)]
pub struct LightningClientModule {
    federation_id: FederationId,
    cfg: LightningClientConfig,
    client_ctx: ClientContext<Self>,
    module_api: FederationApi,
    keypair: Keypair,
    lnurl_keypair: Keypair,
    gateway_conn: Arc<dyn GatewayConnection + Send + Sync>,
    send_executor: ModuleExecutor<SendStateMachine>,
    receive_executor: ModuleExecutor<ReceiveStateMachine>,
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for LightningClientModule {
    type Init = LightningClientInit;
    type Common = LightningModuleTypes;

    async fn start(&self) {
        self.send_executor.start().await;
        self.receive_executor.start().await;
    }

    fn input_fee(
        &self,
        _amount: Amount,
        _input: &<Self::Common as ModuleCommon>::Input,
    ) -> Option<Amount> {
        Some(self.cfg.input_fee)
    }

    fn output_fee(
        &self,
        _amount: Amount,
        _output: &<Self::Common as ModuleCommon>::Output,
    ) -> Option<Amount> {
        Some(self.cfg.output_fee)
    }
}

impl LightningClientModule {
    #[allow(clippy::too_many_arguments)]
    fn new(
        federation_id: FederationId,
        cfg: LightningClientConfig,
        client_ctx: ClientContext<Self>,
        module_api: FederationApi,
        module_root_secret: &DerivableSecret,
        gateway_conn: Arc<dyn GatewayConnection + Send + Sync>,
        task_group: &TaskGroup,
    ) -> Self {
        let sm_context = LightningClientContext {
            federation_id,
            gateway_conn: gateway_conn.clone(),
            client_ctx: client_ctx.clone(),
        };
        let send_executor = ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context.clone(),
            task_group.clone(),
        );
        let receive_executor = ModuleExecutor::new(
            client_ctx.module_db().clone(),
            sm_context,
            task_group.clone(),
        );

        let module = Self {
            federation_id,
            cfg,
            client_ctx,
            module_api,
            keypair: module_root_secret
                .child_key(ChildId(0))
                .to_secp_key(SECP256K1),
            lnurl_keypair: module_root_secret
                .child_key(ChildId(1))
                .to_secp_key(SECP256K1),
            gateway_conn,
            send_executor,
            receive_executor,
        };

        module.spawn_receive_lnurl_task(task_group);

        module.spawn_gateway_map_update_task(task_group);

        module
    }

    fn spawn_gateway_map_update_task(&self, task_group: &TaskGroup) {
        let module = self.clone();

        task_group.spawn_cancellable("gateway_map_update_task", async move {
            module.update_gateway_map().await;
        });
    }

    async fn update_gateway_map(&self) {
        // Update the mapping from lightning node public keys to gateway api
        // endpoints maintained in the module database. When paying an invoice this
        // enables the client to select the gateway that has created the invoice,
        // if possible, such that the payment does not go over lightning, reducing
        // fees and latency.

        if let Ok(gateways) = self.module_api.gateways().await {
            let mut entries = Vec::new();
            for gateway in gateways {
                if let Ok(Some(routing_info)) = self
                    .gateway_conn
                    .routing_info(gateway.clone(), &self.federation_id)
                    .await
                {
                    entries.push((routing_info.lightning_public_key, gateway));
                }
            }

            let dbtx = self.client_ctx.module_db().begin_write().await;
            {
                let tx = dbtx.as_ref();
                for (key, gateway) in entries {
                    tx.insert(&GATEWAY, &GatewayKey(key), &gateway);
                }
            }

            dbtx.commit().await;
        }
    }

    /// Selects an available gateway by querying the federation's registered
    /// gateways, checking if one of them match the invoice's payee public
    /// key, then queries the gateway for `RoutingInfo` to determine if it is
    /// online.
    pub async fn select_gateway(
        &self,
        invoice: Option<Bolt11Invoice>,
    ) -> Result<(SafeUrl, RoutingInfo), SelectGatewayError> {
        let gateways = self
            .module_api
            .gateways()
            .await
            .map_err(|e| SelectGatewayError::FailedToRequestGateways(e.to_string()))?;

        if gateways.is_empty() {
            return Err(SelectGatewayError::NoGatewaysAvailable);
        }

        if let Some(invoice) = invoice
            && let Some(gateway) = self
                .client_ctx
                .module_db()
                .begin_read()
                .await
                .get(&GATEWAY, &GatewayKey(invoice.recover_payee_pub_key()))
                .filter(|gateway| gateways.contains(gateway))
            && let Ok(Some(routing_info)) = self.routing_info(&gateway).await
        {
            return Ok((gateway, routing_info));
        }

        for gateway in gateways {
            if let Ok(Some(routing_info)) = self.routing_info(&gateway).await {
                return Ok((gateway, routing_info));
            }
        }

        Err(SelectGatewayError::GatewaysUnresponsive)
    }

    /// Sends a request to each peer for their registered gateway list and
    /// returns a `Vec<SafeUrl` of all registered gateways to the client.
    pub async fn list_gateways(
        &self,
        peer: Option<PeerId>,
    ) -> Result<Vec<SafeUrl>, ListGatewaysError> {
        if let Some(peer) = peer {
            self.module_api
                .gateways_from_peer(peer)
                .await
                .map_err(|_| ListGatewaysError::FailedToListGateways)
        } else {
            self.module_api
                .gateways()
                .await
                .map_err(|_| ListGatewaysError::FailedToListGateways)
        }
    }

    /// Requests the `RoutingInfo`, including fee information, from the gateway
    /// available at the `SafeUrl`.
    pub async fn routing_info(
        &self,
        gateway: &SafeUrl,
    ) -> Result<Option<RoutingInfo>, RoutingInfoError> {
        self.gateway_conn
            .routing_info(gateway.clone(), &self.federation_id)
            .await
            .map_err(|_| RoutingInfoError::FailedToRequestRoutingInfo)
    }

    /// Pay an invoice. For testing you can optionally specify a gateway to
    /// route with, otherwise a gateway will be selected automatically. If the
    /// invoice was created by a gateway connected to our federation, the same
    /// gateway will be selected to allow for a direct ecash swap. Otherwise we
    /// select a random online gateway.
    ///
    /// The fee for this payment may depend on the selected gateway but
    /// will be limited to one and a half percent plus one hundred satoshis.
    /// This fee accounts for the fee charged by the gateway as well as
    /// the additional fee required to reliably route this payment over
    /// lightning if necessary. Since the gateway has been vetted by at least
    /// one guardian we trust it to set a reasonable fee and only enforce a
    /// rather high limit.
    ///
    /// The absolute fee for a payment can be calculated from the operation meta
    /// to be shown to the user in the transaction history.
    #[allow(clippy::too_many_lines)]
    pub async fn send(
        &self,
        invoice: Bolt11Invoice,
        gateway: Option<SafeUrl>,
    ) -> Result<OperationId, SendPaymentError> {
        let amount = invoice
            .amount_milli_satoshis()
            .ok_or(SendPaymentError::InvoiceMissingAmount)?;

        if invoice.is_expired() {
            return Err(SendPaymentError::InvoiceExpired);
        }

        if self.cfg.network != invoice.currency().into() {
            return Err(SendPaymentError::WrongCurrency {
                invoice_currency: invoice.currency(),
                federation_currency: self.cfg.network.into(),
            });
        }

        let operation_id = OperationId::from_encodable(&invoice.payment_hash());

        let (ephemeral_tweak, ephemeral_pk) = tweak::generate(self.keypair.public_key());

        let refund_keypair = SecretKey::from_slice(&ephemeral_tweak)
            .expect("32 bytes, within curve order")
            .keypair(secp256k1::SECP256K1);

        let (gateway_api, routing_info) = match gateway {
            Some(gateway_api) => (
                gateway_api.clone(),
                self.routing_info(&gateway_api)
                    .await
                    .map_err(|e| SendPaymentError::FailedToConnectToGateway(e.to_string()))?
                    .ok_or(SendPaymentError::FederationNotSupported)?,
            ),
            None => self
                .select_gateway(Some(invoice.clone()))
                .await
                .map_err(SendPaymentError::SelectGateway)?,
        };

        let (send_fee, expiration_delta) = routing_info.send_parameters(&invoice);

        if !send_fee.le(&PaymentFee::SEND_FEE_LIMIT) {
            return Err(SendPaymentError::GatewayFeeExceedsLimit);
        }

        if EXPIRATION_DELTA_LIMIT < expiration_delta {
            return Err(SendPaymentError::GatewayExpirationExceedsLimit);
        }

        let consensus_block_count = self
            .module_api
            .consensus_block_count()
            .await
            .map_err(|e| SendPaymentError::FailedToRequestBlockCount(e.to_string()))?;

        let contract = OutgoingContract {
            payment_image: PaymentImage::Hash(*invoice.payment_hash()),
            amount: send_fee.add_to(amount),
            expiration: consensus_block_count + expiration_delta + CONTRACT_CONFIRMATION_BUFFER,
            claim_pk: routing_info.module_public_key,
            refund_pk: refund_keypair.public_key(),
            ephemeral_pk,
        };

        let client_output = ClientOutput::<LightningOutput> {
            output: LightningOutput::V0(LightningOutputV0::Outgoing(contract.clone())),
            amount: contract.amount,
        };

        let client_output_bundle = self.client_ctx.make_client_outputs(ClientOutputBundle::<
            LightningOutput,
        >::new(vec![
            client_output,
        ]));

        let transaction = TransactionBuilder::new().with_outputs(client_output_bundle);

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        if tx.insert(&SEND_OPERATION, &operation_id, &()).is_some() {
            return Err(SendPaymentError::InvoiceAlreadyAttempted(operation_id));
        }

        let range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(&tx, operation_id, transaction)
            .await
            .map_err(|e| SendPaymentError::FailedToFundPayment(e.to_string()))?;

        self.send_executor
            .add_state_machine_dbtx(
                &tx,
                SendStateMachine {
                    common: SendSMCommon {
                        operation_id,
                        outpoint: OutPoint {
                            txid: range.txid(),
                            out_idx: 0,
                        },
                        contract,
                        gateway_api: Some(gateway_api),
                        invoice: Some(LightningInvoice::Bolt11(invoice.clone())),
                        refund_keypair,
                    },
                    state: SendSMState::Funding,
                },
            )
            .await;

        self.client_ctx
            .log_event(
                &tx,
                operation_id,
                SendPaymentEvent {
                    amount: send_fee.add_to(amount),
                    fee: send_fee.fee(amount),
                },
            )
            .await;

        dbtx.commit().await;

        Ok(operation_id)
    }

    /// Request an invoice. For testing you can optionally specify a gateway to
    /// generate the invoice, otherwise a random online gateway will be selected
    /// automatically.
    ///
    /// The total fee for this payment may depend on the chosen gateway but
    /// will be limited to half of one percent plus fifty satoshis. Since the
    /// selected gateway has been vetted by at least one guardian we trust it to
    /// set a reasonable fee and only enforce a rather high limit.
    ///
    /// The absolute fee for a payment can be calculated from the operation meta
    /// to be shown to the user in the transaction history.
    pub async fn receive(
        &self,
        amount: Amount,
        expiry_secs: u32,
        description: Bolt11InvoiceDescription,
        gateway: Option<SafeUrl>,
    ) -> Result<(Bolt11Invoice, OperationId), ReceiveError> {
        let (_gateway, contract, invoice) = self
            .create_contract_and_fetch_invoice(
                self.keypair.public_key(),
                amount,
                expiry_secs,
                description,
                gateway,
            )
            .await?;

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();

        let operation_id = self
            .receive_incoming_contract(&tx, self.keypair.secret_key(), contract)
            .await
            .expect("The contract has been generated with our public key");

        dbtx.commit().await;

        Ok((invoice, operation_id))
    }

    /// Create an incoming contract locked to a public key derived from the
    /// recipient's static module public key and fetches the corresponding
    /// invoice.
    async fn create_contract_and_fetch_invoice(
        &self,
        recipient_static_pk: PublicKey,
        amount: Amount,
        expiry_secs: u32,
        description: Bolt11InvoiceDescription,
        gateway: Option<SafeUrl>,
    ) -> Result<(SafeUrl, IncomingContract, Bolt11Invoice), ReceiveError> {
        let (ephemeral_tweak, ephemeral_pk) = tweak::generate(recipient_static_pk);

        let encryption_seed = ephemeral_tweak
            .consensus_hash::<sha256::Hash>()
            .to_byte_array();

        let preimage = encryption_seed
            .consensus_hash::<sha256::Hash>()
            .to_byte_array();

        let (gateway, routing_info) = match gateway {
            Some(gateway) => (
                gateway.clone(),
                self.routing_info(&gateway)
                    .await
                    .map_err(|e| ReceiveError::FailedToConnectToGateway(e.to_string()))?
                    .ok_or(ReceiveError::FederationNotSupported)?,
            ),
            None => self
                .select_gateway(None)
                .await
                .map_err(ReceiveError::SelectGateway)?,
        };

        if !routing_info.receive_fee.le(&PaymentFee::RECEIVE_FEE_LIMIT) {
            return Err(ReceiveError::GatewayFeeExceedsLimit);
        }

        let contract_amount = routing_info.receive_fee.subtract_from(amount.msats);

        if contract_amount < MINIMUM_INCOMING_CONTRACT_AMOUNT {
            return Err(ReceiveError::AmountTooSmall);
        }

        let expiration = duration_since_epoch()
            .as_secs()
            .saturating_add(u64::from(expiry_secs));

        let claim_pk = recipient_static_pk
            .mul_tweak(
                secp256k1::SECP256K1,
                &Scalar::from_be_bytes(ephemeral_tweak).expect("Within curve order"),
            )
            .expect("Tweak is valid");

        let contract = IncomingContract::new(
            self.cfg.tpe_agg_pk,
            encryption_seed,
            preimage,
            PaymentImage::Hash(preimage.consensus_hash()),
            contract_amount,
            expiration,
            claim_pk,
            routing_info.module_public_key,
            ephemeral_pk,
        );

        let invoice = self
            .gateway_conn
            .bolt11_invoice(
                gateway.clone(),
                self.federation_id,
                contract.clone(),
                amount,
                description,
                expiry_secs,
            )
            .await
            .map_err(|e| ReceiveError::FailedToConnectToGateway(e.to_string()))?;

        if invoice.payment_hash() != &preimage.consensus_hash() {
            return Err(ReceiveError::InvalidInvoice);
        }

        if invoice.amount_milli_satoshis() != Some(amount.msats) {
            return Err(ReceiveError::IncorrectInvoiceAmount);
        }

        Ok((gateway, contract, invoice))
    }

    // Receive an incoming contract locked to a public key derived from our
    // static module public key. Takes the caller's dbtx so the spawn is atomic
    // with any surrounding state changes (e.g. advancing the lnurl stream
    // index in [`Self::receive_lnurl`]).
    async fn receive_incoming_contract(
        &self,
        dbtx: &WriteTxRef<'_>,
        sk: SecretKey,
        contract: IncomingContract,
    ) -> Option<OperationId> {
        let operation_id = OperationId::from_encodable(&contract.clone());

        let (claim_keypair, agg_decryption_key) = self.recover_contract_keys(sk, &contract)?;

        self.receive_executor
            .add_state_machine_dbtx(
                dbtx,
                ReceiveStateMachine {
                    common: ReceiveSMCommon {
                        operation_id,
                        contract,
                        claim_keypair,
                        agg_decryption_key,
                    },
                    state: ReceiveSMState::Pending,
                },
            )
            .await;

        Some(operation_id)
    }

    fn recover_contract_keys(
        &self,
        sk: SecretKey,
        contract: &IncomingContract,
    ) -> Option<(Keypair, AggregateDecryptionKey)> {
        let tweak = ecdh::SharedSecret::new(&contract.commitment.ephemeral_pk, &sk);

        let encryption_seed = tweak
            .secret_bytes()
            .consensus_hash::<sha256::Hash>()
            .to_byte_array();

        let claim_keypair = sk
            .mul_tweak(&Scalar::from_be_bytes(tweak.secret_bytes()).expect("Within curve order"))
            .expect("Tweak is valid")
            .keypair(secp256k1::SECP256K1);

        if claim_keypair.public_key() != contract.commitment.claim_pk {
            return None; // The claim key is not derived from our pk
        }

        let agg_decryption_key = derive_agg_dk(&self.cfg.tpe_agg_pk, &encryption_seed);

        if !contract.verify_agg_decryption_key(&self.cfg.tpe_agg_pk, &agg_decryption_key) {
            return None; // The decryption key is not derived from our pk
        }

        contract.decrypt_preimage(&agg_decryption_key)?;

        Some((claim_keypair, agg_decryption_key))
    }

    /// Generate an lnurl for the client. You can optionally specify a gateway
    /// to use for testing purposes.
    pub async fn generate_lnurl(
        &self,
        recurringd: SafeUrl,
        gateway: Option<SafeUrl>,
    ) -> Result<String, GenerateLnurlError> {
        let gateways = if let Some(gateway) = gateway {
            vec![gateway]
        } else {
            let gateways = self
                .module_api
                .gateways()
                .await
                .map_err(|e| GenerateLnurlError::FailedToRequestGateways(e.to_string()))?;

            if gateways.is_empty() {
                return Err(GenerateLnurlError::NoGatewaysAvailable);
            }

            gateways
        };

        let payload = picomint_core::base32::encode_prefixed(
            picomint_core::base32::PICOMINT_PREFIX,
            &lnurl::LnurlRequest {
                federation_id: self.federation_id,
                recipient_pk: self.lnurl_keypair.public_key(),
                aggregate_pk: self.cfg.tpe_agg_pk,
                gateways,
            },
        );

        Ok(picomint_lnurl::encode_lnurl(&format!(
            "{recurringd}pay/{payload}"
        )))
    }

    fn spawn_receive_lnurl_task(&self, task_group: &TaskGroup) {
        let module = self.clone();

        task_group.spawn_cancellable("receive_lnurl_task", async move {
            loop {
                module.receive_lnurl().await;
            }
        });
    }

    async fn receive_lnurl(&self) {
        let stream_index = self
            .client_ctx
            .module_db()
            .begin_read()
            .await
            .get(&INCOMING_CONTRACT_STREAM_INDEX, &())
            .unwrap_or(0);

        let (contracts, next_index) = self
            .module_api
            .await_incoming_contracts(stream_index, 128)
            .await;

        let dbtx = self.client_ctx.module_db().begin_write().await;
        let tx = dbtx.as_ref();
        for contract in &contracts {
            self.receive_incoming_contract(&tx, self.lnurl_keypair.secret_key(), contract.clone())
                .await;
        }

        tx.insert(&INCOMING_CONTRACT_STREAM_INDEX, &(), &next_index);

        dbtx.commit().await;
    }
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum SelectGatewayError {
    #[error("Failed to request gateways")]
    FailedToRequestGateways(String),
    #[error("No gateways are available")]
    NoGatewaysAvailable,
    #[error("All gateways failed to respond")]
    GatewaysUnresponsive,
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum SendPaymentError {
    #[error("Invoice is missing an amount")]
    InvoiceMissingAmount,
    #[error("Invoice has expired")]
    InvoiceExpired,
    #[error("A payment for this invoice has already been attempted")]
    InvoiceAlreadyAttempted(OperationId),
    #[error(transparent)]
    SelectGateway(SelectGatewayError),
    #[error("Failed to connect to gateway")]
    FailedToConnectToGateway(String),
    #[error("Gateway does not support this federation")]
    FederationNotSupported,
    #[error("Gateway fee exceeds the allowed limit")]
    GatewayFeeExceedsLimit,
    #[error("Gateway expiration time exceeds the allowed limit")]
    GatewayExpirationExceedsLimit,
    #[error("Failed to request block count")]
    FailedToRequestBlockCount(String),
    #[error("Failed to fund the payment")]
    FailedToFundPayment(String),
    #[error("Invoice is for a different currency")]
    WrongCurrency {
        invoice_currency: Currency,
        federation_currency: Currency,
    },
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum ReceiveError {
    #[error(transparent)]
    SelectGateway(SelectGatewayError),
    #[error("Failed to connect to gateway")]
    FailedToConnectToGateway(String),
    #[error("Gateway does not support this federation")]
    FederationNotSupported,
    #[error("Gateway fee exceeds the allowed limit")]
    GatewayFeeExceedsLimit,
    #[error("Amount is too small to cover fees")]
    AmountTooSmall,
    #[error("Gateway returned an invalid invoice")]
    InvalidInvoice,
    #[error("Gateway returned an invoice with incorrect amount")]
    IncorrectInvoiceAmount,
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum GenerateLnurlError {
    #[error("No gateways are available")]
    NoGatewaysAvailable,
    #[error("Failed to request gateways")]
    FailedToRequestGateways(String),
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum ListGatewaysError {
    #[error("Failed to request gateways")]
    FailedToListGateways,
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum RoutingInfoError {
    #[error("Failed to request routing info")]
    FailedToRequestRoutingInfo,
}
