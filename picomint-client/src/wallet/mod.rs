#![deny(clippy::pedantic)]
#![allow(clippy::missing_errors_doc)]
#![allow(clippy::missing_panics_doc)]
#![allow(clippy::must_use_candidate)]
#![allow(clippy::module_name_repetitions)]

pub use picomint_core::wallet as common;

mod api;
mod db;
pub mod events;
mod send_sm;

use std::collections::BTreeMap;
use std::time::Duration;

use crate::api::FederationResult;
use crate::executor::ModuleExecutor;
use crate::module::ClientContext;
use crate::transaction::{Input, Output, TransactionBuilder};
use anyhow::anyhow;
use bitcoin::address::NetworkUnchecked;
use bitcoin::{Address, ScriptBuf};
use db::{NEXT_OUTPUT_INDEX, VALID_ADDRESS_INDEX};
use events::{ReceiveEvent, SendEvent};
use picomint_core::core::OperationId;
use picomint_core::task::TaskGroup;
use picomint_core::wallet::config::WalletConfigConsensus;
use picomint_core::wallet::{
    StandardScript, WalletInput, WalletOutput, descriptor, is_potential_receive,
};
use picomint_core::wire;
use picomint_core::{Amount, OutPoint, TransactionId};
use picomint_encoding::Encodable;

use crate::secret::Secret;
use picomint_logging::LOG_CLIENT_MODULE_WALLET;
use secp256k1::Keypair;
use send_sm::SendStateMachine;
use thiserror::Error;
use tokio::task::block_in_place;
use tokio::time::sleep;
use tracing::warn;

/// Number of output info entries to scan per batch.
const SLICE_SIZE: u64 = 1000;

#[derive(Encodable)]
enum RootSecretPath {
    Address,
}

#[derive(Debug, Clone)]
pub struct WalletClientModule {
    root_secret: Secret,
    cfg: WalletConfigConsensus,
    client_ctx: ClientContext,
    mint: std::sync::Arc<crate::mint::MintClientModule>,
    send_executor: ModuleExecutor<SendStateMachine>,
}

#[derive(Debug, Clone)]
pub struct WalletClientContext {
    pub client_ctx: ClientContext,
}

impl WalletClientModule {
    pub fn input_fee(&self) -> Amount {
        self.cfg.input_fee
    }

    pub fn output_fee(&self) -> Amount {
        self.cfg.output_fee
    }
}

impl WalletClientModule {
    pub async fn new(
        cfg: WalletConfigConsensus,
        context: ClientContext,
        mint: std::sync::Arc<crate::mint::MintClientModule>,
        module_root_secret: &Secret,
        task_group: &TaskGroup,
    ) -> anyhow::Result<WalletClientModule> {
        let sm_context = WalletClientContext {
            client_ctx: context.clone(),
        };
        let send_executor =
            ModuleExecutor::new(context.db().clone(), sm_context, task_group.clone()).await;

        let module = WalletClientModule {
            root_secret: module_root_secret.clone(),
            cfg,
            client_ctx: context,
            mint,
            send_executor,
        };

        module.spawn_output_scanner(task_group);

        Ok(module)
    }
}

impl WalletClientModule {
    /// Returns the Bitcoin network for this federation.
    pub fn get_network(&self) -> bitcoin::Network {
        self.cfg.network
    }

    /// Fetch the total value of bitcoin controlled by the federation.
    pub async fn total_value(&self) -> FederationResult<bitcoin::Amount> {
        self.client_ctx
            .module_api()
            .wallet_federation_wallet()
            .await
            .map(|tx_out| tx_out.map_or(bitcoin::Amount::ZERO, |tx_out| tx_out.value))
    }

    /// Fetch the consensus block count of the federation.
    pub async fn block_count(&self) -> FederationResult<u64> {
        self.client_ctx
            .module_api()
            .wallet_consensus_block_count()
            .await
    }

    /// Fetch the current consensus feerate.
    pub async fn feerate(&self) -> FederationResult<Option<u64>> {
        self.client_ctx
            .module_api()
            .wallet_consensus_feerate()
            .await
    }

    /// Fetch the current fee required to send an onchain payment.
    pub async fn send_fee(&self) -> Result<bitcoin::Amount, SendError> {
        self.client_ctx
            .module_api()
            .wallet_send_fee()
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
                .client_ctx
                .module_api()
                .wallet_send_fee()
                .await
                .map_err(|e| SendError::FederationError(e.to_string()))?
                .ok_or(SendError::NoConsensusFeerateAvailable)?,
        };

        let operation_id = OperationId::new_random();

        let destination = StandardScript::from_address(&address.clone().assume_checked())
            .ok_or(SendError::UnsupportedAddress)?;

        let tx_builder = TransactionBuilder::from_output(Output {
            output: wire::Output::Wallet(WalletOutput {
                destination,
                value,
                fee,
            }),
            amount: Amount::from_sats((value + fee).to_sat()),
            fee: self.cfg.output_fee,
        });

        let dbtx = self.client_ctx.db().begin_write();

        let txid = self
            .mint
            .finalize_and_submit_transaction(&dbtx.as_ref(), operation_id, tx_builder)
            .map_err(|_| SendError::InsufficientFunds)?;

        let sm = SendStateMachine {
            operation_id,
            outpoint: OutPoint { txid, out_idx: 0 },
            value,
            fee,
        };

        self.send_executor
            .add_state_machine_dbtx(&dbtx.as_ref(), sm);

        let event = SendEvent {
            txid,
            address,
            value,
            fee,
        };

        self.client_ctx
            .log_event(&dbtx.as_ref(), operation_id, event);

        dbtx.commit();

        Ok(operation_id)
    }

    /// Returns the next unused receive address, polling until the initial
    /// address derivation has completed.
    pub async fn receive(&self) -> Address {
        loop {
            let idx = self
                .client_ctx
                .db()
                .begin_read()
                .iter(&VALID_ADDRESS_INDEX, |r| r.next_back().map(|(k, _)| k));

            if let Some(idx) = idx {
                return self.derive_address(idx);
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
            .child(&RootSecretPath::Address)
            .child(&index)
            .to_secp_keypair()
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
    ) -> (OperationId, TransactionId) {
        let operation_id = OperationId::new_random();

        let tx_builder = TransactionBuilder::from_input(Input {
            input: wire::Input::Wallet(WalletInput {
                output_index,
                fee,
                tweak: self.derive_tweak(address_index).public_key(),
            }),
            keypair: self.derive_tweak(address_index),
            amount: Amount::from_sats((value - fee).to_sat()),
            fee: self.cfg.input_fee,
        });

        let dbtx = self.client_ctx.db().begin_write();

        let txid = self
            .mint
            .finalize_and_submit_transaction(&dbtx.as_ref(), operation_id, tx_builder)
            .expect("Input amount is sufficient to finalize transaction");

        let event = ReceiveEvent {
            txid,
            address: self.derive_address(address_index).as_unchecked().clone(),
            value,
            fee,
        };

        self.client_ctx
            .log_event(&dbtx.as_ref(), operation_id, event);

        dbtx.commit();

        (operation_id, txid)
    }

    fn spawn_output_scanner(&self, task_group: &TaskGroup) {
        let module = self.clone();

        task_group.spawn_cancellable("output-scanner", async move {
            let has_seed = module
                .client_ctx
                .db()
                .begin_read()
                .iter(&VALID_ADDRESS_INDEX, |r| r.next().is_some());

            if !has_seed {
                let index = module.next_valid_index(0);
                let dbtx = module.client_ctx.db().begin_write();
                assert!(
                    dbtx.insert(&VALID_ADDRESS_INDEX, &index, &()).is_none(),
                    "seed address index already present"
                );
                dbtx.commit();
            }

            loop {
                match module.check_outputs().await {
                    Ok(skip_wait) => {
                        if skip_wait {
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!(target: LOG_CLIENT_MODULE_WALLET, "Failed to fetch outputs: {e}");
                    }
                }

                sleep(picomint_core::wallet::sleep_duration()).await;
            }
        });
    }

    async fn check_outputs(&self) -> anyhow::Result<bool> {
        let dbtx = self.client_ctx.db().begin_read();

        let next_output_index = dbtx.get(&NEXT_OUTPUT_INDEX, &()).unwrap_or(0);

        let mut valid_indices: Vec<u64> =
            dbtx.iter(&VALID_ADDRESS_INDEX, |r| r.map(|(idx, ())| idx).collect());

        drop(dbtx);

        let mut address_map: BTreeMap<ScriptBuf, u64> = valid_indices
            .iter()
            .map(|&i| (self.derive_address(i).script_pubkey(), i))
            .collect();

        let outputs = self
            .client_ctx
            .module_api()
            .wallet_output_info_slice(next_output_index, next_output_index + SLICE_SIZE)
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

                    let dbtx = self.client_ctx.db().begin_write();

                    dbtx.insert(&VALID_ADDRESS_INDEX, &index, &());

                    dbtx.commit();

                    valid_indices.push(index);

                    address_map.insert(self.derive_address(index).script_pubkey(), index);
                }

                if !output.spent {
                    // In order to not overpay on fees we choose to wait,
                    // the congestion will clear up within a few blocks.
                    if self
                        .client_ctx
                        .module_api()
                        .wallet_pending_tx_chain()
                        .await?
                        .len()
                        >= 3
                    {
                        return Ok(false);
                    }

                    let receive_fee = self
                        .client_ctx
                        .module_api()
                        .wallet_receive_fee()
                        .await?
                        .ok_or(anyhow!("No consensus feerate is available"))?;

                    if output.value > receive_fee {
                        let (operation_id, txid) = self
                            .receive_output(output.index, output.value, address_index, receive_fee)
                            .await;

                        self.client_ctx
                            .await_tx_accepted(operation_id, txid)
                            .await
                            .map_err(|e| anyhow!("Claim transaction was rejected: {e}"))?;
                    }
                }
            }

            let dbtx = self.client_ctx.db().begin_write();

            dbtx.insert(&NEXT_OUTPUT_INDEX, &(), &(output.index + 1));

            dbtx.commit();
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
