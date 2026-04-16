use picomint_api_client::transaction::{TRANSACTION_OVERFLOW_ERROR, TransactionError};
use picomint_core::Amount;
use picomint_core::module::TransactionItemAmounts;

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
    use picomint_core::Amount;
    use picomint_core::module::TransactionItemAmounts;

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
