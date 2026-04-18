use picomint_core::core::OperationId;
use picomint_core::secp256k1::PublicKey;
use picomint_core::util::SafeUrl;
use picomint_encoding::{Decodable, Encodable};
use picomint_redb::table;

#[derive(Debug, Clone, Eq, PartialEq, Hash, Encodable, Decodable)]
pub struct GatewayKey(pub PublicKey);

picomint_redb::consensus_key!(GatewayKey);

table!(
    GATEWAY,
    GatewayKey => SafeUrl,
    "gateway",
);

table!(
    INCOMING_CONTRACT_STREAM_INDEX,
    () => u64,
    "incoming-contract-stream-index",
);

// Tracks that a send operation has been started for this [`OperationId`].
// Used to reject duplicate pay attempts for the same invoice (the op id is
// derived from the invoice payment hash).
table!(
    SEND_OPERATION,
    OperationId => (),
    "send-operation",
);
