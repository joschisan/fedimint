use std::{ffi, iter};

use clap::Parser;
use fedimint_core::Amount;
use fedimint_core::base32::{self, FEDIMINT_PREFIX};
use serde::Serialize;
use serde_json::Value;

use crate::MintClientModule;

#[derive(Parser, Serialize)]
enum Opts {
    /// Count the `ECash` notes in the client's database by denomination.
    Count,
    /// Send `ECash` for the given amount.
    Send { amount: Amount },
    /// Receive the `ECash` by reissuing the notes and return the amount.
    Receive { ecash: String },
}

pub(crate) async fn handle_cli_command(
    mint: &MintClientModule,
    args: &[ffi::OsString],
) -> anyhow::Result<Value> {
    let opts = Opts::parse_from(iter::once(&ffi::OsString::from("mintv2")).chain(args.iter()));

    match opts {
        Opts::Count => Ok(json(mint.get_count_by_denomination().await)),
        Opts::Send { amount } => Ok(json(base32::encode_prefixed(
            FEDIMINT_PREFIX,
            &mint.send(amount, Value::Null).await?,
        ))),
        Opts::Receive { ecash } => Ok(json(
            mint.receive(
                base32::decode_prefixed(FEDIMINT_PREFIX, &ecash)?,
                Value::Null,
            )
            .await?,
        )),
    }
}

fn json<T: Serialize>(value: T) -> Value {
    serde_json::to_value(value).expect("JSON serialization failed")
}
