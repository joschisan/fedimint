use picomint_redb::table;

table!(
    NEXT_OUTPUT_INDEX,
    () => u64,
    "wallet-next-output-index",
);

table!(
    VALID_ADDRESS_INDEX,
    u64 => (),
    "wallet-valid-address-index",
);
