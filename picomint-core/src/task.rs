#![cfg_attr(target_family = "wasm", allow(dead_code))]

/// Just-in-time initialization
pub mod jit;

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use picomint_logging::{LOG_TASK, LOG_TEST};
use thiserror::Error;
use tokio::signal;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use tracing::info;

use crate::runtime;
pub use crate::runtime::*;

/// A group of tasks that can be shut down cooperatively.
///
/// Thin facade over [`TaskTracker`] (join all tasks on shutdown) and
/// [`CancellationToken`] (broadcast a shutdown signal). No panic cascade —
/// a panicked task ends on its own and siblings keep running.
///
/// TODO: remove this type entirely. Target end state:
/// - Spawning only happens in each binary's `main.rs` via a local
///   `TaskTracker` + root `CancellationToken`. No "spawn capability"
///   parameter is threaded through lower layers.
/// - Subsystems expose `run(token: CancellationToken) -> impl Future`
///   (or `serve(...)` etc.) that the top level spawns. They return
///   futures/objects; they do not spawn internally.
/// - Two legitimate "wait for subset" cases use a local `TaskTracker`
///   + child token: the AlephBFT session scope and the `run_config_gen`
///   setup phase. One acceptable leak below `main`: `run_iroh_api`'s
///   per-connection fan-out needs a `TaskTracker` passed in.
/// - Client library holds `CancellationToken` + `Vec<JoinHandle<()>>`
///   in its `Client` struct. `impl Drop` cancels the token and aborts
///   the handles. No tracker on the client — OS-kill is the normal
///   case and join is impossible across mobile process termination.
/// - Add `panic = "abort"` to the daemon binaries' release profile
///   to compensate for the lost panic cascade.
#[derive(Clone, Default, Debug)]
pub struct TaskGroup {
    tracker: TaskTracker,
    token: CancellationToken,
}

impl TaskGroup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn make_handle(&self) -> TaskHandle {
        TaskHandle {
            token: self.token.clone(),
        }
    }

    /// Create a sub-group that shares nothing with the parent except a
    /// child cancellation token — canceling the parent cancels the child,
    /// but the sub-group's tracker is independent so `join_all` on it
    /// waits only for its own tasks.
    pub fn make_subgroup(&self) -> Self {
        Self {
            tracker: TaskTracker::new(),
            token: self.token.child_token(),
        }
    }

    pub fn is_shutting_down(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn shutdown(&self) {
        self.token.cancel();
    }

    pub async fn shutdown_join_all(
        self,
        join_timeout: impl Into<Option<Duration>>,
    ) -> Result<(), anyhow::Error> {
        self.shutdown();
        self.join_all(join_timeout.into()).await
    }

    pub fn install_kill_handler(&self) {
        let token = self.token.clone();
        runtime::spawn("kill handlers", async move {
            let ctrl_c = async {
                signal::ctrl_c()
                    .await
                    .expect("failed to install Ctrl+C handler");
            };

            #[cfg(unix)]
            let terminate = async {
                signal::unix::signal(signal::unix::SignalKind::terminate())
                    .expect("failed to install signal handler")
                    .recv()
                    .await;
            };

            #[cfg(not(unix))]
            let terminate = std::future::pending::<()>();

            tokio::select! {
                () = ctrl_c => {},
                () = terminate => {},
            }

            info!(
                target: LOG_TASK,
                "signal received, starting graceful shutdown"
            );
            token.cancel();
        });
    }

    pub fn spawn<Fut, R>(
        &self,
        _name: impl Into<String>,
        f: impl FnOnce(TaskHandle) -> Fut + Send + 'static,
    ) where
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        let handle = self.make_handle();
        self.tracker.spawn(f(handle));
    }

    pub fn spawn_silent<Fut, R>(
        &self,
        name: impl Into<String>,
        f: impl FnOnce(TaskHandle) -> Fut + Send + 'static,
    ) where
        Fut: Future<Output = R> + Send + 'static,
        R: Send + 'static,
    {
        self.spawn(name, f);
    }

    pub fn spawn_cancellable<R>(
        &self,
        _name: impl Into<String>,
        future: impl Future<Output = R> + Send + 'static,
    ) where
        R: Send + 'static,
    {
        let token = self.token.clone();
        self.tracker.spawn(async move {
            let _ = token.run_until_cancelled(future).await;
        });
    }

    pub fn spawn_cancellable_silent<R>(
        &self,
        name: impl Into<String>,
        future: impl Future<Output = R> + Send + 'static,
    ) where
        R: Send + 'static,
    {
        self.spawn_cancellable(name, future);
    }

    pub async fn join_all(self, timeout: Option<Duration>) -> Result<(), anyhow::Error> {
        self.tracker.close();
        match timeout {
            Some(d) => {
                let _ = tokio::time::timeout(d, self.tracker.wait()).await;
            }
            None => self.tracker.wait().await,
        }
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct TaskHandle {
    token: CancellationToken,
}

#[derive(Error, Debug, Clone)]
#[error("Task group is shutting down")]
#[non_exhaustive]
pub struct ShuttingDownError {}

impl TaskHandle {
    pub fn is_shutting_down(&self) -> bool {
        self.token.is_cancelled()
    }

    pub fn make_shutdown_rx(&self) -> TaskShutdownToken {
        let token = self.token.clone();
        TaskShutdownToken(Box::pin(async move { token.cancelled_owned().await }))
    }

    pub async fn cancel_on_shutdown<F: Future>(
        &self,
        fut: F,
    ) -> Result<F::Output, ShuttingDownError> {
        let token = self.token.clone();
        token
            .run_until_cancelled_owned(fut)
            .await
            .ok_or(ShuttingDownError {})
    }
}

pub struct TaskShutdownToken(Pin<Box<dyn Future<Output = ()> + Send>>);

impl Future for TaskShutdownToken {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.0.as_mut().poll(cx)
    }
}

// Used in tests when sleep functionality is desired so it can be logged.
// Must include comment describing the reason for sleeping.
pub async fn sleep_in_test(comment: impl AsRef<str>, duration: Duration) {
    info!(
        target: LOG_TEST,
        "Sleeping for {}.{:03} seconds because: {}",
        duration.as_secs(),
        duration.subsec_millis(),
        comment.as_ref()
    );
    sleep(duration).await;
}

/// An error used as a "cancelled" marker in [`Cancellable`].
#[derive(Error, Debug)]
#[error("Operation cancelled")]
pub struct Cancelled;

/// Operation that can potentially get cancelled returning no result (e.g.
/// program shutdown).
pub type Cancellable<T> = std::result::Result<T, Cancelled>;
