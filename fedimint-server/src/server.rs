//! Concrete `Server` container for the fixed module set.
//!
//! Holds one module per known kind (mint/ln/wallet) and match-dispatches on
//! the wire enum variant. Each slot still stores an `Arc<dyn IServerModule>`
//! so the existing module-init + registry machinery can populate it without
//! touching downcasts; the wire-level dispatch is static (variant → field),
//! even though the final trait call is virtual.

use fedimint_api_client::transaction::Transaction;
use fedimint_api_client::wire::{self, LN_INSTANCE_ID, MINT_INSTANCE_ID, WALLET_INSTANCE_ID};
use fedimint_core::module::InputMeta;
use fedimint_core::module::audit::Audit;
use fedimint_core::{InPoint, OutPoint, PeerId};
use fedimint_redb::{ReadTxRef, WriteTransaction, WriteTxRef};
use fedimint_server_core::{DynServerModule, ServerModuleRegistry};

#[derive(Clone)]
pub struct Server {
    pub mint: DynServerModule,
    pub ln: DynServerModule,
    pub wallet: DynServerModule,
}

impl Server {
    /// Build a `Server` from an existing `ServerModuleRegistry`. Each of the
    /// three canonical instance ids must be present.
    pub fn from_registry(modules: &ServerModuleRegistry) -> Self {
        Self {
            mint: modules.get_expect(MINT_INSTANCE_ID).clone(),
            ln: modules.get_expect(LN_INSTANCE_ID).clone(),
            wallet: modules.get_expect(WALLET_INSTANCE_ID).clone(),
        }
    }

    fn by_instance_id(&self, id: fedimint_core::core::ModuleInstanceId) -> &DynServerModule {
        match id {
            MINT_INSTANCE_ID => &self.mint,
            LN_INSTANCE_ID => &self.ln,
            WALLET_INSTANCE_ID => &self.wallet,
            other => panic!("unknown module instance id: {other}"),
        }
    }

    pub async fn consensus_proposal(
        &self,
        dbtx: &ReadTxRef<'_>,
    ) -> Vec<wire::ModuleConsensusItem> {
        let mut items = Vec::new();
        items.extend(
            self.mint
                .consensus_proposal(&dbtx.isolate(format!("module-{MINT_INSTANCE_ID}")), MINT_INSTANCE_ID)
                .await,
        );
        items.extend(
            self.ln
                .consensus_proposal(&dbtx.isolate(format!("module-{LN_INSTANCE_ID}")), LN_INSTANCE_ID)
                .await,
        );
        items.extend(
            self.wallet
                .consensus_proposal(&dbtx.isolate(format!("module-{WALLET_INSTANCE_ID}")), WALLET_INSTANCE_ID)
                .await,
        );
        items
    }

    pub async fn process_consensus_item(
        &self,
        dbtx: &WriteTxRef<'_>,
        item: &wire::ModuleConsensusItem,
        peer_id: PeerId,
    ) -> anyhow::Result<()> {
        let module = self.by_instance_id(item.module_instance_id());
        module.process_consensus_item(dbtx, item, peer_id).await
    }

    pub async fn process_input(
        &self,
        dbtx: &WriteTxRef<'_>,
        input: &wire::Input,
        in_point: InPoint,
    ) -> Result<InputMeta, wire::InputError> {
        let module = self.by_instance_id(input.module_instance_id());
        module.process_input(dbtx, input, in_point).await
    }

    pub async fn process_output(
        &self,
        dbtx: &WriteTxRef<'_>,
        output: &wire::Output,
        out_point: OutPoint,
    ) -> Result<fedimint_core::module::TransactionItemAmounts, wire::OutputError> {
        let module = self.by_instance_id(output.module_instance_id());
        module.process_output(dbtx, output, out_point).await
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
    tx: &fedimint_redb::WriteTransaction,
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
