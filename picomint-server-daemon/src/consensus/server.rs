//! Concrete `Server` container for the fixed module set.
//!
//! Holds typed instances of the three canonical modules and match-dispatches
//! on the wire enum variant directly — no trait indirection.

use std::sync::Arc;

use picomint_core::module::InputMeta;
use picomint_core::module::audit::Audit;
use picomint_core::transaction::Transaction;
use picomint_core::wire;
use picomint_core::{InPoint, OutPoint, PeerId};
use picomint_redb::{WriteTransaction, WriteTxRef};

use crate::consensus::ln::Lightning;
use crate::consensus::mint::Mint;
use crate::consensus::wallet::Wallet;

/// Per-module database isolation namespaces. Each `Server` method scopes its
/// view through [`picomint_redb::ReadTxRef::isolate`] / [`WriteTxRef::isolate`]
/// so modules never see anything outside their own keyspace.
pub const MINT_NS: &str = "mint";
pub const LN_NS: &str = "ln";
pub const WALLET_NS: &str = "wallet";

#[derive(Clone)]
pub struct Server {
    pub mint: Arc<Mint>,
    pub ln: Arc<Lightning>,
    pub wallet: Arc<Wallet>,
}

impl Server {
    pub async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        item: &wire::ModuleConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        match item {
            wire::ModuleConsensusItem::Mint(ci) => match *ci {},
            wire::ModuleConsensusItem::Ln(ci) => {
                self.ln
                    .process_consensus_item(&dbtx.isolate(LN_NS.to_string()), ci.clone(), peer_id)
                    .await
            }
            wire::ModuleConsensusItem::Wallet(ci) => {
                self.wallet
                    .process_consensus_item(
                        &dbtx.isolate(WALLET_NS.to_string()),
                        ci.clone(),
                        peer_id,
                    )
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
                .process_input(&dbtx.isolate(MINT_NS.to_string()), i, in_point)
                .await
                .map_err(wire::InputError::Mint),
            wire::Input::Ln(i) => self
                .ln
                .process_input(&dbtx.isolate(LN_NS.to_string()), i, in_point)
                .await
                .map_err(wire::InputError::Ln),
            wire::Input::Wallet(i) => self
                .wallet
                .process_input(&dbtx.isolate(WALLET_NS.to_string()), i, in_point)
                .await
                .map_err(wire::InputError::Wallet),
        }
    }

    pub async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &wire::Output,
        out_point: OutPoint,
    ) -> Result<picomint_core::module::TransactionItemAmounts, wire::OutputError> {
        match output {
            wire::Output::Mint(o) => self
                .mint
                .process_output(&dbtx.isolate(MINT_NS.to_string()), o, out_point)
                .await
                .map_err(wire::OutputError::Mint),
            wire::Output::Ln(o) => self
                .ln
                .process_output(&dbtx.isolate(LN_NS.to_string()), o, out_point)
                .await
                .map_err(wire::OutputError::Ln),
            wire::Output::Wallet(o) => self
                .wallet
                .process_output(&dbtx.isolate(WALLET_NS.to_string()), o, out_point)
                .await
                .map_err(wire::OutputError::Wallet),
        }
    }

    pub async fn audit(&self, dbtx: &WriteTransaction, audit: &mut Audit) {
        self.mint
            .audit(&dbtx.isolate(MINT_NS.to_string()), audit)
            .await;
        self.ln.audit(&dbtx.isolate(LN_NS.to_string()), audit).await;
        self.wallet
            .audit(&dbtx.isolate(WALLET_NS.to_string()), audit)
            .await;
    }
}

/// Dispatch the inputs and outputs of a transaction to the relevant modules.
pub async fn process_transaction_with_server(
    server: &Server,
    tx: &WriteTransaction,
    transaction: &Transaction,
) -> Result<(), picomint_core::transaction::TransactionError> {
    use picomint_core::transaction::TransactionError;

    use crate::consensus::transaction::FundingVerifier;

    let mut funding_verifier = FundingVerifier::default();
    let mut public_keys = Vec::new();

    let txid = transaction.tx_hash();

    for (input, in_idx) in transaction.inputs.iter().zip(0u64..) {
        let meta = server
            .process_input(&tx.as_ref(), input, InPoint { txid, in_idx })
            .await
            .map_err(TransactionError::Input)?;

        funding_verifier.add_input(meta.amount)?;
        public_keys.push(meta.pub_key);
    }

    transaction.validate_signatures(&public_keys)?;

    for (output, out_idx) in transaction.outputs.iter().zip(0u64..) {
        let amount = server
            .process_output(&tx.as_ref(), output, OutPoint { txid, out_idx })
            .await
            .map_err(TransactionError::Output)?;

        funding_verifier.add_output(amount)?;
    }

    funding_verifier.verify_funding()?;

    Ok(())
}
