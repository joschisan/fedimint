use std::sync::Arc;
use std::time::Duration;

use fedimint_core::db::IDatabaseTransactionOpsCoreTyped;
use fedimint_core::runtime::sleep;
use fedimint_core::util::FmtCompact as _;
use fedimint_logging::LOG_CLIENT;
use tracing::debug;

use crate::Client;
use crate::db::ExpirationStatusKey;

pub(crate) async fn run_expiration_status_task(client: Arc<Client>) {
    loop {
        match client.api.expiration_status().await {
            Ok(status) => {
                let mut dbtx = client.db().begin_transaction().await;

                match status {
                    Some(s) => {
                        dbtx.insert_entry(&ExpirationStatusKey, &s).await;
                    }
                    None => {
                        dbtx.remove_entry(&ExpirationStatusKey).await;
                    }
                }

                dbtx.commit_tx().await;
            }
            Err(err) => {
                debug!(
                    target: LOG_CLIENT,
                    err = %err.fmt_compact(),
                    "Failed to fetch expiration status"
                );
            }
        }

        sleep(Duration::from_secs(86400)).await; // Check once a day
    }
}
