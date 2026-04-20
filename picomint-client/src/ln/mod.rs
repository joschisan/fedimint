#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::module_name_repetitions)]
#![allow(clippy::must_use_candidate)]

pub use picomint_core::ln as common;

mod api;
mod db;
pub mod events;
mod gateway_http;
mod send_sm;

use std::sync::Arc;

use crate::executor::ModuleExecutor;
use crate::module::ClientContext;
use crate::transaction::{Input, Output, TransactionBuilder};
use bitcoin::secp256k1;
use db::{GATEWAY, GatewayKey, INCOMING_CONTRACT_STREAM_INDEX, SEND_OPERATION};
use lightning_invoice::{Bolt11Invoice, Currency};
use picomint_core::config::FederationId;
use picomint_core::core::OperationId;
use picomint_core::ln::config::LightningConfigConsensus;
use picomint_core::ln::contracts::{IncomingContract, OutgoingContract, PaymentImage};
use picomint_core::ln::gateway_api::{PaymentFee, RoutingInfo};
use picomint_core::ln::{
    Bolt11InvoiceDescription, IncomingContractPath, LightningInput, LightningInvoice,
    LightningOutput, MINIMUM_INCOMING_CONTRACT_AMOUNT, lnurl,
};
use picomint_core::task::TaskGroup;
use picomint_core::wire;

#[derive(picomint_encoding::Encodable)]
enum RootSecretPath {
    Receive,
    Refund,
}
use crate::secret::Secret;
use picomint_core::time::duration_since_epoch;
use picomint_core::util::SafeUrl;
use picomint_core::{Amount, OutPoint, PeerId};
use picomint_encoding::Encodable;
use picomint_redb::WriteTxRef;
use secp256k1::{Keypair, PublicKey, SecretKey, ecdh};
use thiserror::Error;
use tpe::{AggregateDecryptionKey, derive_agg_dk};

use self::events::{ReceiveEvent, SendEvent};
use self::send_sm::{SendSMCommon, SendSMState, SendStateMachine};

/// Number of blocks until outgoing lightning contracts times out and user
/// client can refund it unilaterally
const EXPIRATION_DELTA_LIMIT: u64 = 1440;

/// A two hour buffer in case either the client or gateway go offline
const CONTRACT_CONFIRMATION_BUFFER: u64 = 12;

pub type SendResult = Result<OperationId, SendPaymentError>;

#[derive(Debug, Clone)]
pub struct LightningClientContext {
    pub(crate) federation_id: FederationId,
    pub(crate) client_ctx: ClientContext,
    pub(crate) mint: Arc<crate::mint::MintClientModule>,
    pub(crate) input_fee: Amount,
}

#[derive(Debug, Clone)]
pub struct LightningClientModule {
    federation_id: FederationId,
    cfg: LightningConfigConsensus,
    client_ctx: ClientContext,
    mint: Arc<crate::mint::MintClientModule>,
    module_root_secret: Secret,
    send_executor: ModuleExecutor<SendStateMachine>,
}

impl LightningClientModule {
    pub fn input_fee(&self) -> Amount {
        self.cfg.input_fee
    }

    pub fn output_fee(&self) -> Amount {
        self.cfg.output_fee
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        federation_id: FederationId,
        cfg: LightningConfigConsensus,
        client_ctx: ClientContext,
        mint: Arc<crate::mint::MintClientModule>,
        module_root_secret: &Secret,
        task_group: &TaskGroup,
    ) -> anyhow::Result<Self> {
        let sm_context = LightningClientContext {
            federation_id,
            client_ctx: client_ctx.clone(),
            mint: mint.clone(),
            input_fee: cfg.input_fee,
        };
        let send_executor =
            ModuleExecutor::new(client_ctx.db().clone(), sm_context, task_group.clone()).await;

        let module = Self {
            federation_id,
            cfg,
            client_ctx,
            mint,
            module_root_secret: *module_root_secret,
            send_executor,
        };

        module.spawn_receive_scan_task(task_group);

        module.spawn_gateway_map_update_task(task_group);

        Ok(module)
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

        if let Ok(gateways) = self.client_ctx.module_api().ln_gateways().await {
            let mut entries = Vec::new();
            for gateway in gateways {
                if let Ok(Some(routing_info)) =
                    gateway_http::routing_info(gateway.clone(), &self.federation_id).await
                {
                    entries.push((routing_info.lightning_public_key, gateway));
                }
            }

            let dbtx = self.client_ctx.db().begin_write();
            {
                let tx = dbtx.as_ref();
                for (key, gateway) in entries {
                    tx.insert(&GATEWAY, &GatewayKey(key), &gateway);
                }
            }

            dbtx.commit();
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
            .client_ctx
            .module_api()
            .ln_gateways()
            .await
            .map_err(|e| SelectGatewayError::FailedToRequestGateways(e.to_string()))?;

        if gateways.is_empty() {
            return Err(SelectGatewayError::NoGatewaysAvailable);
        }

        if let Some(invoice) = invoice
            && let Some(gateway) = self
                .client_ctx
                .db()
                .begin_read()
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
            self.client_ctx
                .module_api()
                .ln_gateways_from_peer(peer)
                .await
                .map_err(|_| ListGatewaysError::FailedToListGateways)
        } else {
            self.client_ctx
                .module_api()
                .ln_gateways()
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
        gateway_http::routing_info(gateway.clone(), &self.federation_id)
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

        let tweak: [u8; 16] = rand::Rng::r#gen(&mut rand::thread_rng());

        let refund_keypair = self
            .module_root_secret
            .child(&RootSecretPath::Refund)
            .child(&tweak)
            .to_secp_keypair();

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
            .client_ctx
            .module_api()
            .ln_consensus_block_count()
            .await
            .map_err(|e| SendPaymentError::FailedToRequestBlockCount(e.to_string()))?;

        let contract = OutgoingContract {
            payment_image: PaymentImage::Hash(*invoice.payment_hash()),
            amount: send_fee.add_to(amount),
            expiration: consensus_block_count + expiration_delta + CONTRACT_CONFIRMATION_BUFFER,
            claim_pk: routing_info.module_public_key,
            refund_pk: refund_keypair.public_key(),
            tweak,
        };

        let tx_builder = TransactionBuilder::from_output(Output {
            output: wire::Output::Ln(LightningOutput::Outgoing(contract.clone())),
            amount: contract.amount,
            fee: self.cfg.output_fee,
        });

        let dbtx = self.client_ctx.db().begin_write();

        if dbtx
            .as_ref()
            .insert(&SEND_OPERATION, &operation_id, &())
            .is_some()
        {
            return Err(SendPaymentError::InvoiceAlreadyAttempted(operation_id));
        }

        let txid = self
            .mint
            .finalize_and_submit_transaction(&dbtx.as_ref(), operation_id, tx_builder)
            .map_err(|e| SendPaymentError::FailedToFundPayment(e.to_string()))?;

        let sm = SendStateMachine {
            common: SendSMCommon {
                operation_id,
                outpoint: OutPoint { txid, out_idx: 0 },
                contract,
                gateway_api: Some(gateway_api),
                invoice: Some(LightningInvoice::Bolt11(invoice.clone())),
                refund_keypair,
            },
            state: SendSMState::Funding,
        };

        self.send_executor
            .add_state_machine_dbtx(&dbtx.as_ref(), sm);

        let event = SendEvent {
            txid,
            amount: send_fee.add_to(amount),
            fee: send_fee.fee(amount),
        };

        self.client_ctx
            .log_event(&dbtx.as_ref(), operation_id, event);

        dbtx.commit();

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
    ) -> Result<Bolt11Invoice, ReceiveError> {
        let receive_keypair = self
            .module_root_secret
            .child(&RootSecretPath::Receive)
            .to_secp_keypair();

        self.create_contract_and_fetch_invoice(
            receive_keypair.public_key(),
            amount,
            expiry_secs,
            description,
            gateway,
        )
        .await
    }

    /// Create an incoming contract locked to a public key derived from the
    /// recipient's static module public key and fetches the corresponding
    /// invoice.
    async fn create_contract_and_fetch_invoice(
        &self,
        recipient_pk: PublicKey,
        amount: Amount,
        expiry_secs: u32,
        description: Bolt11InvoiceDescription,
        gateway: Option<SafeUrl>,
    ) -> Result<Bolt11Invoice, ReceiveError> {
        let ephemeral_kp = Keypair::new(secp256k1::SECP256K1, &mut rand::thread_rng());

        let shared_secret = ecdh::SharedSecret::new(&recipient_pk, &ephemeral_kp.secret_key());

        let contract_secret = Secret::new_root(&shared_secret.secret_bytes());

        let encryption_seed = contract_secret
            .child(&IncomingContractPath::EncryptionSeed)
            .to_bytes();

        let preimage = contract_secret
            .child(&IncomingContractPath::Preimage)
            .to_bytes();

        let claim_tweak = contract_secret
            .child(&IncomingContractPath::ClaimKey)
            .to_secp_scalar();

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

        let claim_pk = recipient_pk
            .mul_tweak(secp256k1::SECP256K1, &claim_tweak)
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
            ephemeral_kp.public_key(),
        );

        let invoice = gateway_http::bolt11_invoice(
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

        Ok(invoice)
    }

    /// Try to claim a streamed incoming contract: decrypt with the caller's
    /// secret key, and if it's ours submit the claim input + log the
    /// `ReceiveEvent` in the caller's dbtx (which also advances the scanner's
    /// stream index atomically).
    fn receive_incoming_contract(
        &self,
        dbtx: &WriteTxRef<'_>,
        sk: SecretKey,
        outpoint: OutPoint,
        contract: &IncomingContract,
    ) {
        let Some((claim_keypair, agg_dk)) = self.recover_contract_keys(sk, contract) else {
            return;
        };

        let tx_builder = TransactionBuilder::from_input(Input {
            input: wire::Input::Ln(LightningInput::Incoming(outpoint, agg_dk)),
            keypair: claim_keypair,
            amount: contract.commitment.amount,
            fee: self.cfg.input_fee,
        });

        let operation_id = OperationId::from_encodable(&outpoint);

        let txid = self
            .mint
            .finalize_and_submit_transaction(dbtx, operation_id, tx_builder)
            .expect("Cannot claim input, additional funding needed");

        let event = ReceiveEvent {
            txid,
            amount: contract.commitment.amount,
        };

        self.client_ctx.log_event(dbtx, operation_id, event);
    }

    fn recover_contract_keys(
        &self,
        sk: SecretKey,
        contract: &IncomingContract,
    ) -> Option<(Keypair, AggregateDecryptionKey)> {
        let shared_secret =
            ecdh::SharedSecret::new(&contract.commitment.ephemeral_pk, &sk).secret_bytes();

        let contract_secret = Secret::new_root(&shared_secret);

        let encryption_seed = contract_secret
            .child(&IncomingContractPath::EncryptionSeed)
            .to_bytes();

        let claim_tweak = contract_secret
            .child(&IncomingContractPath::ClaimKey)
            .to_secp_scalar();

        let claim_keypair = sk
            .mul_tweak(&claim_tweak)
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
                .client_ctx
                .module_api()
                .ln_gateways()
                .await
                .map_err(|e| GenerateLnurlError::FailedToRequestGateways(e.to_string()))?;

            if gateways.is_empty() {
                return Err(GenerateLnurlError::NoGatewaysAvailable);
            }

            gateways
        };

        let receive_keypair = self
            .module_root_secret
            .child(&RootSecretPath::Receive)
            .to_secp_keypair();

        let payload = picomint_base32::encode(&lnurl::LnurlRequest {
            federation_id: self.federation_id,
            recipient_pk: receive_keypair.public_key(),
            aggregate_pk: self.cfg.tpe_agg_pk,
            gateways,
        });

        Ok(picomint_lnurl::encode_lnurl(&format!(
            "{recurringd}pay/{payload}"
        )))
    }

    fn spawn_receive_scan_task(&self, task_group: &TaskGroup) {
        let module = self.clone();

        task_group.spawn_cancellable("receive_scan_task", async move {
            loop {
                module.receive_scan().await;
            }
        });
    }

    async fn receive_scan(&self) {
        let stream_index = self
            .client_ctx
            .db()
            .begin_read()
            .get(&INCOMING_CONTRACT_STREAM_INDEX, &())
            .unwrap_or(0);

        let (entries, next_index) = self
            .client_ctx
            .module_api()
            .ln_await_incoming_contracts(stream_index, 128)
            .await;

        let sk = self
            .module_root_secret
            .child(&RootSecretPath::Receive)
            .to_secp_keypair()
            .secret_key();

        let dbtx = self.client_ctx.db().begin_write();

        for (outpoint, contract) in &entries {
            self.receive_incoming_contract(&dbtx.as_ref(), sk, *outpoint, contract);
        }

        dbtx.insert(&INCOMING_CONTRACT_STREAM_INDEX, &(), &next_index);

        dbtx.commit();
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
