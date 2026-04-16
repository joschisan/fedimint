//! Concrete `Server` container for the fixed module set.
//!
//! Holds typed instances of the three canonical modules and match-dispatches on
//! the wire enum variant. There is no dyn dispatch: the server-side module
//! trait (`IServerModule`) has been deleted and replaced with direct calls to
//! the concrete `ServerModule` impls on `Mint`, `Lightning`, and `Wallet`.

use std::sync::Arc;

use fedimint_api_client::transaction::Transaction;
use fedimint_api_client::wire::{self, LN_INSTANCE_ID, MINT_INSTANCE_ID, WALLET_INSTANCE_ID};
use fedimint_core::module::InputMeta;
use fedimint_core::module::audit::Audit;
use fedimint_core::{InPoint, OutPoint, PeerId};
use fedimint_lnv2_server::Lightning;
use fedimint_mintv2_server::Mint;
use fedimint_redb::{ReadTxRef, WriteTransaction, WriteTxRef};
use fedimint_server_core::ServerModule;
use fedimint_walletv2_server::Wallet;

#[derive(Clone)]
pub struct Server {
    pub mint: Arc<Mint>,
    pub ln: Arc<Lightning>,
    pub wallet: Arc<Wallet>,
}

impl Server {
    pub async fn consensus_proposal(&self, dbtx: &ReadTxRef<'_>) -> Vec<wire::ModuleConsensusItem> {
        let mut items = Vec::new();
        items.extend(
            self.mint
                .consensus_proposal(&dbtx.isolate(format!("module-{MINT_INSTANCE_ID}")))
                .await
                .into_iter()
                .map(wire::ModuleConsensusItem::Mint),
        );
        items.extend(
            self.ln
                .consensus_proposal(&dbtx.isolate(format!("module-{LN_INSTANCE_ID}")))
                .await
                .into_iter()
                .map(wire::ModuleConsensusItem::Ln),
        );
        items.extend(
            self.wallet
                .consensus_proposal(&dbtx.isolate(format!("module-{WALLET_INSTANCE_ID}")))
                .await
                .into_iter()
                .map(wire::ModuleConsensusItem::Wallet),
        );
        items
    }

    pub async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        item: &wire::ModuleConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        match item {
            wire::ModuleConsensusItem::Mint(ci) => {
                self.mint
                    .process_consensus_item(dbtx, ci.clone(), peer_id)
                    .await
            }
            wire::ModuleConsensusItem::Ln(ci) => {
                self.ln
                    .process_consensus_item(dbtx, ci.clone(), peer_id)
                    .await
            }
            wire::ModuleConsensusItem::Wallet(ci) => {
                self.wallet
                    .process_consensus_item(dbtx, ci.clone(), peer_id)
                    .await
            }
        }
    }

    pub async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &wire::Input,
        in_point: InPoint,
    ) -> Result<InputMeta, wire::InputError> {
        match input {
            wire::Input::Mint(i) => self
                .mint
                .process_input(dbtx, i, in_point)
                .await
                .map_err(wire::InputError::Mint),
            wire::Input::Ln(i) => self
                .ln
                .process_input(dbtx, i, in_point)
                .await
                .map_err(wire::InputError::Ln),
            wire::Input::Wallet(i) => self
                .wallet
                .process_input(dbtx, i, in_point)
                .await
                .map_err(wire::InputError::Wallet),
        }
    }

    pub async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &wire::Output,
        out_point: OutPoint,
    ) -> Result<fedimint_core::module::TransactionItemAmounts, wire::OutputError> {
        match output {
            wire::Output::Mint(o) => self
                .mint
                .process_output(dbtx, o, out_point)
                .await
                .map_err(wire::OutputError::Mint),
            wire::Output::Ln(o) => self
                .ln
                .process_output(dbtx, o, out_point)
                .await
                .map_err(wire::OutputError::Ln),
            wire::Output::Wallet(o) => self
                .wallet
                .process_output(dbtx, o, out_point)
                .await
                .map_err(wire::OutputError::Wallet),
        }
    }

    pub async fn audit(&self, dbtx: &WriteTransaction, audit: &mut Audit) {
        self.mint
            .audit(
                &dbtx.isolate(format!("module-{MINT_INSTANCE_ID}")),
                audit,
                MINT_INSTANCE_ID,
            )
            .await;
        self.ln
            .audit(
                &dbtx.isolate(format!("module-{LN_INSTANCE_ID}")),
                audit,
                LN_INSTANCE_ID,
            )
            .await;
        self.wallet
            .audit(
                &dbtx.isolate(format!("module-{WALLET_INSTANCE_ID}")),
                audit,
                WALLET_INSTANCE_ID,
            )
            .await;
    }
}

/// Dispatch the inputs and outputs of a transaction to the relevant modules.
pub async fn process_transaction_with_server(
    server: &Server,
    tx: &WriteTransaction,
    transaction: &Transaction,
) -> Result<(), fedimint_api_client::transaction::TransactionError> {
    use fedimint_api_client::transaction::TransactionError;

    use crate::consensus::transaction::FundingVerifier;

    let mut funding_verifier = FundingVerifier::default();
    let mut public_keys = Vec::new();

    let txid = transaction.tx_hash();

    for (input, in_idx) in transaction.inputs.iter().zip(0u64..) {
        let instance_id = input.module_instance_id();
        let view = tx.isolate(format!("module-{instance_id}"));

        let meta = server
            .process_input(&view, input, InPoint { txid, in_idx })
            .await
            .map_err(TransactionError::Input)?;

        funding_verifier.add_input(meta.amount)?;
        public_keys.push(meta.pub_key);
    }

    transaction.validate_signatures(&public_keys)?;

    for (output, out_idx) in transaction.outputs.iter().zip(0u64..) {
        let instance_id = output.module_instance_id();
        let view = tx.isolate(format!("module-{instance_id}"));

        let amount = server
            .process_output(&view, output, OutPoint { txid, out_idx })
            .await
            .map_err(TransactionError::Output)?;

        funding_verifier.add_output(amount)?;
    }

    funding_verifier.verify_funding()?;

    Ok(())
}
