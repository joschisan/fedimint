use fedimint_core::OutPoint;
use fedimint_core::db::v2::TableDef;
use fedimint_core::secp256k1::PublicKey;
use fedimint_mintv2_common::{Denomination, RecoveryItem};
use tbs::{BlindedMessage, BlindedSignatureShare};

pub const NOTE_NONCE: TableDef<PublicKey, ()> = TableDef::new("note_nonce");

pub const BLINDED_SIGNATURE_SHARE: TableDef<OutPoint, BlindedSignatureShare> =
    TableDef::new("blinded_signature_share");

pub const BLINDED_SIGNATURE_SHARE_RECOVERY: TableDef<BlindedMessage, BlindedSignatureShare> =
    TableDef::new("blinded_signature_share_recovery");

pub const ISSUANCE_COUNTER: TableDef<Denomination, u64> = TableDef::new("issuance_counter");

pub const RECOVERY_ITEM: TableDef<u64, RecoveryItem> = TableDef::new("recovery_item");
