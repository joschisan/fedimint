use std::time::Duration;

pub use backon::{Backoff, FibonacciBackoff};
use backon::{BackoffBuilder, FibonacciBuilder};

/// Fibonacci backoff with jitter for network-facing retry loops.
///
/// Starts at 250ms, caps at 10s between attempts, never gives up.
pub fn networking_backoff() -> FibonacciBackoff {
    FibonacciBuilder::default()
        .with_jitter()
        .with_min_delay(Duration::from_millis(250))
        .with_max_delay(Duration::from_secs(10))
        .with_max_times(usize::MAX)
        .build()
}
