#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::module_name_repetitions)]

pub use fedimint_walletv2_common as common;

mod api;
mod db;
pub mod events;
mod receive_sm;
mod send_sm;

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use anyhow::anyhow;
use api::WalletFederationApi;
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, ScriptBuf};
use db::{NextOutputIndexKey, ValidAddressIndexKey, ValidAddressIndexPrefix};
use events::{ReceivePaymentEvent, SendPaymentEvent};
use fedimint_api_client::api::{DynModuleApi, FederationResult};
use fedimint_client::transaction::{
    ClientInput, ClientInputBundle, ClientOutput, ClientOutputBundle, TransactionBuilder,
};
use fedimint_client_module::db::ClientModuleMigrationFn;
use fedimint_client_module::executor::ModuleExecutor;
use fedimint_client_module::module::init::{ClientModuleInit, ClientModuleInitArgs};
use fedimint_client_module::module::{ClientContext, ClientModule};
use fedimint_core::core::OperationId;
use fedimint_core::db::{
    Database, DatabaseVersion, IReadDatabaseTransactionOpsTyped, IWriteDatabaseTransactionOpsTyped,
    WriteDatabaseTransaction,
};
use fedimint_core::encoding::Encodable;
use fedimint_core::module::{ModuleCommon, ModuleInit};
use fedimint_core::task::{TaskGroup, block_in_place, sleep};
use fedimint_core::{Amount, OutPoint, TransactionId, apply, async_trait_maybe_send};
use fedimint_derive_secret::{ChildId, DerivableSecret};
use fedimint_logging::LOG_CLIENT_MODULE_WALLETV2;
use fedimint_walletv2_common::config::WalletClientConfig;
use fedimint_walletv2_common::{
    StandardScript, WalletCommonInit, WalletInput, WalletInputV0, WalletModuleTypes, WalletOutput,
    WalletOutputV0, descriptor, is_potential_receive,
};
use futures::StreamExt;
use receive_sm::{ReceiveSMCommon, ReceiveSMState, ReceiveStateMachine};
use secp256k1::Keypair;
use send_sm::{SendSMCommon, SendSMState, SendStateMachine};
use strum::IntoEnumIterator as _;
use thiserror::Error;
use tracing::warn;

/// Number of output info entries to scan per batch.
const SLICE_SIZE: u64 = 1000;

#[derive(Debug, Clone)]
pub struct WalletClientModule {
    root_secret: DerivableSecret,
    cfg: WalletClientConfig,
    client_ctx: ClientContext<Self>,
    db: Database,
    module_api: DynModuleApi,
    send_executor: ModuleExecutor<SendStateMachine>,
    receive_executor: ModuleExecutor<ReceiveStateMachine>,
}

#[derive(Debug, Clone)]
pub struct WalletClientContext {
    pub client_ctx: ClientContext<WalletClientModule>,
}

#[apply(async_trait_maybe_send!)]
impl ClientModule for WalletClientModule {
    type Init = WalletClientInit;
    type Common = WalletModuleTypes;

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

#[derive(Debug, Clone, Default)]
pub struct WalletClientInit;

impl ModuleInit for WalletClientInit {
    type Common = WalletCommonInit;

}

#[apply(async_trait_maybe_send!)]
impl ClientModuleInit for WalletClientInit {
    type Module = WalletClientModule;

    async fn init(&self, args: &ClientModuleInitArgs<Self>) -> anyhow::Result<Self::Module> {
        let client_ctx = args.context();
        let sm_context = WalletClientContext {
            client_ctx: client_ctx.clone(),
        };
        let send_executor = ModuleExecutor::new(
            args.db().clone(),
            sm_context.clone(),
            args.task_group().clone(),
        );
        let receive_executor =
            ModuleExecutor::new(args.db().clone(), sm_context, args.task_group().clone());

        let module = WalletClientModule {
            root_secret: args.module_root_secret().clone(),
            cfg: args.cfg().clone(),
            client_ctx,
            db: args.db().clone(),
            module_api: args.module_api().clone(),
            send_executor,
            receive_executor,
        };

        module.spawn_output_scanner(args.task_group());

        Ok(module)
    }

    fn get_database_migrations(&self) -> BTreeMap<DatabaseVersion, ClientModuleMigrationFn> {
        BTreeMap::new()
    }

    fn used_db_prefixes(&self) -> Option<BTreeSet<u8>> {
        Some(db::DbKeyPrefix::iter().map(|p| p as u8).collect())
    }
}

impl WalletClientModule {
    /// Returns the Bitcoin network for this federation.
    pub fn get_network(&self) -> bitcoin::Network {
        self.cfg.network
    }

    /// Fetch the total value of bitcoin controlled by the federation.
    pub async fn total_value(&self) -> FederationResult<bitcoin::Amount> {
        self.module_api
            .federation_wallet()
            .await
            .map(|tx_out| tx_out.map_or(bitcoin::Amount::ZERO, |tx_out| tx_out.value))
    }

    /// Fetch the consensus block count of the federation.
    pub async fn block_count(&self) -> FederationResult<u64> {
        self.module_api.consensus_block_count().await
    }

    /// Fetch the current consensus feerate.
    pub async fn feerate(&self) -> FederationResult<Option<u64>> {
        self.module_api.consensus_feerate().await
    }

    /// Fetch the current fee required to send an onchain payment.
    pub async fn send_fee(&self) -> Result<bitcoin::Amount, SendError> {
        self.module_api
            .send_fee()
            .await
            .map_err(|e| SendError::FederationError(e.to_string()))?
            .ok_or(SendError::NoConsensusFeerateAvailable)
    }

    /// Send an onchain payment with the given fee.
    pub async fn send(
        &self,
        address: Address<NetworkUnchecked>,
        value: bitcoin::Amount,
        fee: Option<bitcoin::Amount>,
    ) -> Result<OperationId, SendError> {
        if !address.is_valid_for_network(self.cfg.network) {
            return Err(SendError::WrongNetwork);
        }

        if value < self.cfg.dust_limit {
            return Err(SendError::DustValue);
        }

        let fee = match fee {
            Some(value) => value,
            None => self
                .module_api
                .send_fee()
                .await
                .map_err(|e| SendError::FederationError(e.to_string()))?
                .ok_or(SendError::NoConsensusFeerateAvailable)?,
        };

        let operation_id = OperationId::new_random();

        let destination = StandardScript::from_address(&address.clone().assume_checked())
            .ok_or(SendError::UnsupportedAddress)?;

        let client_output = ClientOutput::<WalletOutput> {
            output: WalletOutput::V0(WalletOutputV0 {
                destination,
                value,
                fee,
            }),
            amount: Amount::from_sats((value + fee).to_sat()),
        };

        let client_output_bundle =
            self.client_ctx
                .make_client_outputs(ClientOutputBundle::<WalletOutput>::new(vec![client_output]));

        let mut dbtx = self.client_ctx.module_db().begin_write_transaction().await;

        let range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(
                &mut dbtx.to_ref_nc(),
                operation_id,
                TransactionBuilder::new().with_outputs(client_output_bundle),
            )
            .await
            .map_err(|_| SendError::InsufficientFunds)?;

        self.send_executor
            .add_state_machine_dbtx(
                &mut dbtx.to_ref_nc(),
                SendStateMachine {
                    common: SendSMCommon {
                        operation_id,
                        outpoint: OutPoint {
                            txid: range.txid(),
                            out_idx: 0,
                        },
                        value,
                        fee,
                    },
                    state: SendSMState::Funding,
                },
            )
            .await;

        self.client_ctx
            .log_event(
                &mut dbtx,
                SendPaymentEvent {
                    operation_id,
                    address,
                    value,
                    fee,
                },
            )
            .await;

        dbtx.commit_tx().await;

        Ok(operation_id)
    }

    /// Returns the next unused receive address, polling until the initial
    /// address derivation has completed.
    pub async fn receive(&self) -> Address {
        loop {
            if let Some(entry) = self
                .db
                .begin_write_transaction()
                .await
                .find_by_prefix_sorted_descending(&ValidAddressIndexPrefix)
                .await
                .next()
                .await
            {
                return self.derive_address(entry.0.0);
            }

            sleep(Duration::from_secs(1)).await;
        }
    }

    fn derive_address(&self, index: u64) -> Address {
        descriptor(
            &self.cfg.bitcoin_pks,
            &self.derive_tweak(index).public_key().consensus_hash(),
        )
        .address(self.cfg.network)
    }

    fn derive_tweak(&self, index: u64) -> Keypair {
        self.root_secret
            .child_key(ChildId(index))
            .to_secp_key(secp256k1::SECP256K1)
    }

    /// Find the next valid index starting from (and including) `start_index`.
    #[allow(clippy::maybe_infinite_iter)]
    fn next_valid_index(&self, start_index: u64) -> u64 {
        let pks_hash = self.cfg.bitcoin_pks.consensus_hash();

        block_in_place(|| {
            (start_index..)
                .find(|i| is_potential_receive(&self.derive_address(*i).script_pubkey(), &pks_hash))
                .expect("Will always find a valid index")
        })
    }

    /// Issue ecash for an unspent output with a given fee.
    async fn receive_output(
        &self,
        output_index: u64,
        value: bitcoin::Amount,
        address_index: u64,
        fee: bitcoin::Amount,
    ) -> TransactionId {
        let operation_id = OperationId::new_random();

        let client_input = ClientInput::<WalletInput> {
            input: WalletInput::V0(WalletInputV0 {
                output_index,
                fee,
                tweak: self.derive_tweak(address_index).public_key(),
            }),
            keys: vec![self.derive_tweak(address_index)],
            amount: Amount::from_sats((value - fee).to_sat()),
        };

        let client_input_bundle =
            self.client_ctx
                .make_client_inputs(ClientInputBundle::<WalletInput>::new(vec![client_input]));

        let mut dbtx = self.client_ctx.module_db().begin_write_transaction().await;

        let range = self
            .client_ctx
            .finalize_and_submit_transaction_dbtx(
                &mut dbtx.to_ref_nc(),
                operation_id,
                TransactionBuilder::new().with_inputs(client_input_bundle),
            )
            .await
            .expect("Input amount is sufficient to finalize transaction");

        self.receive_executor
            .add_state_machine_dbtx(
                &mut dbtx.to_ref_nc(),
                ReceiveStateMachine {
                    common: ReceiveSMCommon {
                        operation_id,
                        txid: range.txid(),
                        value,
                        fee,
                    },
                    state: ReceiveSMState::Funding,
                },
            )
            .await;

        self.client_ctx
            .log_event(
                &mut dbtx,
                ReceivePaymentEvent {
                    operation_id,
                    address: self.derive_address(address_index).as_unchecked().clone(),
                    value,
                    fee,
                },
            )
            .await;

        dbtx.commit_tx().await;

        range.txid()
    }

    fn spawn_output_scanner(&self, task_group: &TaskGroup) {
        let module = self.clone();

        task_group.spawn_cancellable("output-scanner", async move {
            let needs_seed = module
                .db
                .begin_write_transaction()
                .await
                .find_by_prefix(&ValidAddressIndexPrefix)
                .await
                .next()
                .await
                .is_none();

            if needs_seed {
                let index = module.next_valid_index(0);
                let mut dbtx = module.db.begin_write_transaction().await;
                dbtx.insert_new_entry(&ValidAddressIndexKey(index), &())
                    .await;
                dbtx.commit_tx().await;
            }

            loop {
                match module.check_outputs().await {
                    Ok(skip_wait) => {
                        if skip_wait {
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!(target: LOG_CLIENT_MODULE_WALLETV2, "Failed to fetch outputs: {e}");
                    }
                }

                sleep(fedimint_walletv2_common::sleep_duration()).await;
            }
        });
    }

    async fn check_outputs(&self) -> anyhow::Result<bool> {
        let mut dbtx = self.db.begin_write_transaction().await;

        let next_output_index = dbtx.get_value(&NextOutputIndexKey).await.unwrap_or(0);

        let mut valid_indices: Vec<u64> = dbtx
            .find_by_prefix(&ValidAddressIndexPrefix)
            .await
            .map(|entry| entry.0.0)
            .collect()
            .await;

        drop(dbtx);

        let mut address_map: BTreeMap<ScriptBuf, u64> = valid_indices
            .iter()
            .map(|&i| (self.derive_address(i).script_pubkey(), i))
            .collect();

        let outputs = self
            .module_api
            .output_info_slice(next_output_index, next_output_index + SLICE_SIZE)
            .await?;

        for output in &outputs {
            if let Some(&address_index) = address_map.get(&output.script) {
                let next_address_index = valid_indices
                    .last()
                    .copied()
                    .expect("we have at least one address index");

                // If we used the highest valid index, add the next valid one
                if address_index == next_address_index {
                    let index = self.next_valid_index(next_address_index + 1);

                    let mut dbtx = self.db.begin_write_transaction().await;

                    dbtx.insert_entry(&ValidAddressIndexKey(index), &()).await;

                    dbtx.commit_tx_result().await?;

                    valid_indices.push(index);

                    address_map.insert(self.derive_address(index).script_pubkey(), index);
                }

                if !output.spent {
                    // In order to not overpay on fees we choose to wait,
                    // the congestion will clear up within a few blocks.
                    if self.module_api.pending_tx_chain().await?.len() >= 3 {
                        return Ok(false);
                    }

                    let receive_fee = self
                        .module_api
                        .receive_fee()
                        .await?
                        .ok_or(anyhow!("No consensus feerate is available"))?;

                    if output.value > receive_fee {
                        let txid = self
                            .receive_output(output.index, output.value, address_index, receive_fee)
                            .await;

                        self.client_ctx
                            .await_tx_accepted(txid)
                            .await
                            .map_err(|e| anyhow!("Claim transaction was rejected: {e}"))?;
                    }
                }
            }

            let mut dbtx = self.db.begin_write_transaction().await;

            dbtx.insert_entry(&NextOutputIndexKey, &(output.index + 1))
                .await;

            dbtx.commit_tx_result().await?;
        }

        Ok(!outputs.is_empty())
    }
}

#[derive(Error, Debug, Clone, Eq, PartialEq)]
pub enum SendError {
    #[error("Address is from a different network than the federation.")]
    WrongNetwork,
    #[error("The value is too small")]
    DustValue,
    #[error("Federation returned an error: {0}")]
    FederationError(String),
    #[error("No consensus feerate is available at this time")]
    NoConsensusFeerateAvailable,
    #[error("The client does not have sufficient funds to send the payment")]
    InsufficientFunds,
    #[error("Unsupported address type")]
    UnsupportedAddress,
}
