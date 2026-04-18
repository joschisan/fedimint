use picomint_encoding::{Decodable, Encodable};
use picomint_core::secp256k1::PublicKey;
use picomint_core::OutPoint;
use picomint_redb::table;
use picomint_core::mint::{Denomination, RecoveryItem};
use tbs::{BlindedMessage, BlindedSignatureShare};

/// Newtype wrapper used as the key of [`NOTE_NONCE`] so we can give it a redb
/// `Key` impl locally (foreign `PublicKey` can't).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encodable, Decodable)]
pub struct NoteNonceKey(pub PublicKey);

picomint_redb::consensus_key!(NoteNonceKey);

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
