use fedimint_core::db::DatabaseTransaction;
use fedimint_core::module::TransactionItemAmounts;
use fedimint_core::transaction::{TRANSACTION_OVERFLOW_ERROR, Transaction, TransactionError};
use fedimint_core::{Amount, InPoint, OutPoint};
use fedimint_server_core::ServerModuleRegistry;
use rayon::iter::{IntoParallelIterator, ParallelIterator};

#[derive(Debug, PartialEq, Eq)]
pub enum TxProcessingMode {
    Submission,
    Consensus,
}

pub async fn process_transaction_with_dbtx(
    modules: ServerModuleRegistry,
    dbtx: &mut DatabaseTransaction<'_>,
    transaction: &Transaction,
    mode: TxProcessingMode,
) -> Result<(), TransactionError> {
    // We can not return the error here as errors are not returned in a specified
    // order and the client still expects consensus on the error. Since the
    // error is not extensible at the moment we need to incorrectly return the
    // InvalidWitnessLength variant.
    transaction
        .inputs
        .clone()
        .into_par_iter()
        .try_for_each(|input| {
            modules
                .get_expect(input.module_instance_id())
                .verify_input(&input)
        })
        .map_err(|_| TransactionError::InvalidWitnessLength)?;

    let mut funding_verifier = FundingVerifier::default();
    let mut public_keys = Vec::new();

    let txid = transaction.tx_hash();

    for (input, in_idx) in transaction.inputs.iter().zip(0u64..) {
        // somewhat unfortunately, we need to do the extra checks berofe `process_x`
        // does the changes in the dbtx
        if mode == TxProcessingMode::Submission {
            modules
                .get_expect(input.module_instance_id())
                .verify_input_submission(
                    &mut dbtx
                        .to_ref_with_prefix_module_id(input.module_instance_id())
                        .0,
                    input,
                )
                .await
                .map_err(TransactionError::Input)?;
        }
        let meta = modules
            .get_expect(input.module_instance_id())
            .process_input(
                &mut dbtx
                    .to_ref_with_prefix_module_id(input.module_instance_id())
                    .0,
                input,
                InPoint { txid, in_idx },
            )
            .await
            .map_err(TransactionError::Input)?;

        funding_verifier.add_input(meta.amount)?;
        public_keys.push(meta.pub_key);
    }

    transaction.validate_signatures(&public_keys)?;

    for (output, out_idx) in transaction.outputs.iter().zip(0u64..) {
        // somewhat unfortunately, we need to do the extra checks berofe `process_x`
        // does the changes in the dbtx
        if mode == TxProcessingMode::Submission {
            modules
                .get_expect(output.module_instance_id())
                .verify_output_submission(
                    &mut dbtx
                        .to_ref_with_prefix_module_id(output.module_instance_id())
                        .0,
                    output,
                    OutPoint { txid, out_idx },
                )
                .await
                .map_err(TransactionError::Output)?;
        }

        let amount = modules
            .get_expect(output.module_instance_id())
            .process_output(
                &mut dbtx
                    .to_ref_with_prefix_module_id(output.module_instance_id())
                    .0,
                output,
                OutPoint { txid, out_idx },
            )
            .await
            .map_err(TransactionError::Output)?;

        funding_verifier.add_output(amount)?;
    }

    funding_verifier.verify_funding()?;

    Ok(())
}

#[derive(Clone, Debug, Default)]
pub struct FundingVerifier {
    inputs: Amount,
    outputs: Amount,
    fees: Amount,
}

impl FundingVerifier {
    pub fn add_input(
        &mut self,
        input: TransactionItemAmounts,
    ) -> Result<&mut Self, TransactionError> {
        self.inputs = self
            .inputs
            .checked_add(input.amount)
            .ok_or(TRANSACTION_OVERFLOW_ERROR)?;
        self.fees = self
            .fees
            .checked_add(input.fee)
            .ok_or(TRANSACTION_OVERFLOW_ERROR)?;

        Ok(self)
    }

    pub fn add_output(
        &mut self,
        output_amounts: TransactionItemAmounts,
    ) -> Result<&mut Self, TransactionError> {
        self.outputs = self
            .outputs
            .checked_add(output_amounts.amount)
            .ok_or(TRANSACTION_OVERFLOW_ERROR)?;
        self.fees = self
            .fees
            .checked_add(output_amounts.fee)
            .ok_or(TRANSACTION_OVERFLOW_ERROR)?;

        Ok(self)
    }

    pub fn verify_funding(self) -> Result<(), TransactionError> {
        let outputs_and_fees = self
            .outputs
            .checked_add(self.fees)
            .ok_or(TRANSACTION_OVERFLOW_ERROR)?;

        if self.inputs < outputs_and_fees {
            return Err(TransactionError::UnbalancedTransaction {
                inputs: self.inputs,
                outputs: self.outputs,
                fee: self.fees,
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests;
