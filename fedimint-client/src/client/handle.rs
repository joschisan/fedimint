use std::ops;
use std::sync::Arc;
use std::time::Duration;

use fedimint_core::runtime;
use fedimint_core::util::FmtCompactAnyhow as _;
use fedimint_logging::LOG_CLIENT;
use tracing::{debug, warn};

use super::Client;

/// User handle to the [`Client`] instance
///
/// On drop of [`ClientHandle`] the client's executor is stopped and the client
/// task group is joined.
///
/// Notably it [`ops::Deref`]s to the [`Client`] where most methods live.
///
/// Put this in an Arc to clone it (see [`ClientHandleArc`]).
#[derive(Debug)]
pub struct ClientHandle {
    inner: Option<Arc<Client>>,
}

/// An alias for a reference counted [`ClientHandle`]
pub type ClientHandleArc = Arc<ClientHandle>;

impl ClientHandle {
    pub(crate) fn new(inner: Arc<Client>) -> Self {
        ClientHandle {
            inner: inner.into(),
        }
    }

    pub fn start_executor(&self) {
        // All module state machines now run on per-module `ModuleExecutor`s
        // started eagerly by `ClientModule::start`. Kept as a no-op so
        // external callers don't have to care.
    }
}

impl ops::Deref for ClientHandle {
    type Target = Client;

    fn deref(&self) -> &Self::Target {
        self.inner.as_ref().expect("Must have inner client set")
    }
}

/// Stop the executor and join the client task group when the last handle is
/// dropped. The executor holds an `Arc<Client>`, so without explicitly stopping
/// it here the client would never be dropped.
impl Drop for ClientHandle {
    fn drop(&mut self) {
        let Some(inner) = self.inner.take() else {
            return;
        };
        let task_group = inner.task_group.clone();
        drop(inner);

        debug!(target: LOG_CLIENT, "Shutting down the Client on last handle drop");
        // nosemgrep: ban-raw-block-on
        runtime::block_in_place(|| {
            runtime::block_on(async {
                if let Err(err) = task_group
                    .shutdown_join_all(Some(Duration::from_secs(30)))
                    .await
                {
                    warn!(target: LOG_CLIENT, err = %err.fmt_compact_anyhow(), "Error waiting for client task group to shut down");
                }
            });
        });
    }
}
