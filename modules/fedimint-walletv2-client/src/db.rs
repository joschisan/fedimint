use fedimint_core::table;

table!(
    NEXT_OUTPUT_INDEX,
    () => u64,
    "next-output-index",
);

table!(
    VALID_ADDRESS_INDEX,
    u64 => (),
    "valid-address-index",
);
