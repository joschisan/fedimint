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
