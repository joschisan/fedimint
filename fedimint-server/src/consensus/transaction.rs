use fedimint_core::db::DatabaseTransaction;
use fedimint_core::module::TransactionItemAmounts;
use fedimint_core::transaction::{TRANSACTION_OVERFLOW_ERROR, Transaction, TransactionError};
use fedimint_core::{Amount, InPoint, OutPoint};
use fedimint_server_core::ServerModuleRegistry;

pub async fn process_transaction_with_dbtx(
    modules: ServerModuleRegistry,
    dbtx: &mut DatabaseTransaction<'_>,
    transaction: &Transaction,
) -> Result<(), TransactionError> {
    let mut funding_verifier = FundingVerifier::default();
    let mut public_keys = Vec::new();

    let txid = transaction.tx_hash();

    for (input, in_idx) in transaction.inputs.iter().zip(0u64..) {
        let meta = modules
            .get_expect(input.module_instance_id())
            .process_input(
                &mut dbtx.to_ref_with_prefix_module_id(input.module_instance_id()),
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
        let amount = modules
            .get_expect(output.module_instance_id())
            .process_output(
                &mut dbtx.to_ref_with_prefix_module_id(output.module_instance_id()),
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

        if self.inputs >= outputs_and_fees {
            return Ok(());
        }

        Err(TransactionError::UnbalancedTransaction {
            inputs: self.inputs,
            outputs: self.outputs,
            fee: self.fees,
        })
    }
}

#[cfg(test)]
mod tests {
    use fedimint_core::Amount;
    use fedimint_core::module::TransactionItemAmounts;

    #[test]
    fn sanity_test_funding_verifier() {
        let mut v = super::FundingVerifier::default();

        v.add_input(TransactionItemAmounts {
            amount: Amount::from_msats(3),
            fee: Amount::from_msats(1),
        })
        .unwrap()
        .add_output(TransactionItemAmounts {
            amount: Amount::from_msats(1),
            fee: Amount::from_msats(1),
        })
        .unwrap();

        assert!(v.clone().verify_funding().is_ok());

        v.add_output(TransactionItemAmounts {
            amount: Amount::from_msats(1),
            fee: Amount::ZERO,
        })
        .unwrap();

        assert!(v.clone().verify_funding().is_err());

        v.add_input(TransactionItemAmounts {
            amount: Amount::from_msats(10),
            fee: Amount::ZERO,
        })
        .unwrap();

        // Overfunding is always allowed
        assert!(v.clone().verify_funding().is_ok());
    }
}
