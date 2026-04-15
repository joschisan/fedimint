use fedimint_core::db::v2::TableDef;
use fedimint_core::util::SafeUrl;
use fedimint_core::{OutPoint, PeerId};
use fedimint_lnv2_common::ContractId;
use fedimint_lnv2_common::contracts::{IncomingContract, OutgoingContract};
use tpe::DecryptionKeyShare;

pub const BLOCK_COUNT_VOTE: TableDef<PeerId, u64> = TableDef::new("block_count_vote");

pub const UNIX_TIME_VOTE: TableDef<PeerId, u64> = TableDef::new("unix_time_vote");

pub const INCOMING_CONTRACT: TableDef<OutPoint, IncomingContract> =
    TableDef::new("incoming_contract");

pub const INCOMING_CONTRACT_OUTPOINT: TableDef<ContractId, OutPoint> =
    TableDef::new("incoming_contract_outpoint");

pub const OUTGOING_CONTRACT: TableDef<OutPoint, OutgoingContract> =
    TableDef::new("outgoing_contract");

pub const DECRYPTION_KEY_SHARE: TableDef<OutPoint, DecryptionKeyShare> =
    TableDef::new("decryption_key_share");

pub const PREIMAGE: TableDef<OutPoint, [u8; 32]> = TableDef::new("preimage");

pub const GATEWAY: TableDef<SafeUrl, ()> = TableDef::new("gateway");

/// Incoming contracts are indexed in three ways:
/// 1) A sequential stream mapping: `stream_index (u64)` -> `IncomingContract`
///    This enables efficient streaming reads using range queries on
///    `INCOMING_CONTRACT_STREAM`.
/// 2) A global monotonically increasing index: `INCOMING_CONTRACT_STREAM_INDEX`
///    -> `u64`. This stores the next stream index to be assigned and is used to
///    wait for new incoming contracts to arrive.
/// 3) A reverse lookup from `OutPoint` -> `stream_index` (via
///    `INCOMING_CONTRACT_INDEX`). This allows finding a specific incoming
///    contract's stream position by its `OutPoint`, while still supporting
///    sequential reads via the stream prefix. This is used to remove the
///    contract from the stream once it has been spent.
///
/// The combination allows both random access (by `OutPoint`) and ordered
/// iteration over all unspent incoming contracts (by `stream_index`).
pub const INCOMING_CONTRACT_STREAM_INDEX: TableDef<(), u64> =
    TableDef::new("incoming_contract_stream_index");

pub const INCOMING_CONTRACT_STREAM: TableDef<u64, IncomingContract> =
    TableDef::new("incoming_contract_stream");

pub const INCOMING_CONTRACT_INDEX: TableDef<OutPoint, u64> =
    TableDef::new("incoming_contract_index");
