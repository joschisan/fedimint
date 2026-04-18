use std::time::Duration;

use backon::FibonacciBuilder;
pub use backon::{BackoffBuilder, FibonacciBackoff, Retryable};

/// Fibonacci backoff builder with jitter for network-facing retry loops.
///
/// Starts at 250ms, caps at 10s between attempts, never gives up. Pair with
/// [`Retryable::retry`] at the call site.
pub fn networking_backoff() -> FibonacciBuilder {
    FibonacciBuilder::default()
        .with_jitter()
        .with_min_delay(Duration::from_millis(250))
        .with_max_delay(Duration::from_secs(10))
        .with_max_times(usize::MAX)
}
