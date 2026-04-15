use fedimint_core::core::OperationId;
use fedimint_core::table;

// Tracks that an operation has already been started for this
// [`OperationId`]. Each gateway entry point (send/relay) derives its op id
// deterministically from the contract, so we use this to reject duplicate
// attempts.
table!(
    OPERATION,
    OperationId => (),
    "operation",
);
