use std::str::FromStr;
use std::time::Duration;

use tracing::warn;

pub const DISCOVER_API_VERSION_TIMEOUT_ENV: &str = "DISCOVER_API_VERSION_TIMEOUT";

#[cfg(not(target_family = "wasm"))]
pub fn get_discover_api_version_timeout() -> Duration {
    use picomint_core::util::FmtCompact as _;

    if let Ok(s) = std::env::var(DISCOVER_API_VERSION_TIMEOUT_ENV) {
        match FromStr::from_str(&s) {
            Ok(secs) => return Duration::from_secs(secs),
            Err(err) => warn!(
                err = %err.fmt_compact(),
                var = DISCOVER_API_VERSION_TIMEOUT_ENV,
                "Could not parse env variable"
            ),
        }
    }
    Duration::from_mins(1)
}

#[cfg(target_family = "wasm")]
pub fn get_discover_api_version_timeout() -> Duration {
    Duration::from_secs(60)
}
