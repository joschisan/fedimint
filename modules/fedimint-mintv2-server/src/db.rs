use fedimint_core::encoding::{Decodable, Encodable};
use fedimint_core::secp256k1::PublicKey;
use fedimint_core::{table, OutPoint};
use fedimint_mintv2_common::{Denomination, RecoveryItem};
use tbs::{BlindedMessage, BlindedSignatureShare};

/// Newtype wrapper used as the key of [`NOTE_NONCE`] so we can give it a redb
/// `Key` impl locally (foreign `PublicKey` can't).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encodable, Decodable)]
pub struct NoteNonceKey(pub PublicKey);

fedimint_core::consensus_key!(NoteNonceKey);

table!(
    NOTE_NONCE,
    NoteNonceKey => (),
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
