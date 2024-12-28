use std::{ffi, iter};

use clap::Parser;
use fedimint_core::Amount;
use serde::Serialize;
use serde_json::Value;

use crate::{ECash, MintClientModule};

#[derive(Parser, Serialize)]
enum Opts {
    /// Count the ECash notes in the client's database by denomination.
    Count,
    /// Send ECash for the given amount with an optional description.
    Send {
        amount: Amount,
        #[arg(long)]
        memo: Option<String>,
    },
    /// Receive the ECash by reissuing the notes and return the total amount of
    /// the ecash. This method is idempotent.
    Receive { ecash: String },
}

pub(crate) async fn handle_cli_command(
    mint: &MintClientModule,
    args: &[ffi::OsString],
) -> anyhow::Result<Value> {
    let opts = Opts::parse_from(iter::once(&ffi::OsString::from("mintv2")).chain(args.iter()));

    match opts {
        Opts::Count => Ok(json(mint.get_count_by_denomination().await)),
        Opts::Send { amount, memo } => Ok(json(mint.send(amount, memo).await?.encode_base58())),
        Opts::Receive { ecash } => Ok(json(mint.receive(ECash::decode_base58(&ecash)?).await?)),
    }
}

fn json<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("JSON serialization failed")
}
