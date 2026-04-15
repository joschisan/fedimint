use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{OutPoint, table};
use fedimint_mintv2_common::{Denomination, RecoveryItem};
use tbs::{BlindedMessage, BlindedSignatureShare};

table!(
    NOTE_NONCE,
    PublicKey => (),
    "note-nonce",
);

table!(
    BLINDED_SIGNATURE_SHARE,
    OutPoint => BlindedSignatureShare,
    "blinded-signature-share",
);

table!(
    BLINDED_SIGNATURE_SHARE_RECOVERY,
    BlindedMessage => BlindedSignatureShare,
    "blinded-signature-share-recovery",
);

table!(
    ISSUANCE_COUNTER,
    Denomination => u64,
    "issuance-counter",
);

table!(
    RECOVERY_ITEM,
    u64 => RecoveryItem,
    "recovery-item",
);
