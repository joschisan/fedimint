pub mod backoff_util;

use std::fmt::{Debug, Display, Formatter};
use std::future::Future;
use std::hash::Hash;
use std::io::Write;
use std::path::Path;
use std::pin::Pin;
use std::str::FromStr;
use std::sync::LazyLock;
use std::{fs, io};

use anyhow::format_err;
use futures::StreamExt;
use picomint_logging::LOG_CORE;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use tracing::{debug, warn};
use url::{Host, ParseError, Url};

use crate::envs::{DEBUG_SHOW_SECRETS_ENV, is_env_var_set};
use crate::runtime;

/// Future that is `Send` unless targeting WASM
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a + Send>>;

/// Stream that is `Send` unless targeting WASM
pub type BoxStream<'a, T> = Pin<Box<dyn futures::Stream<Item = T> + 'a + Send>>;

#[async_trait::async_trait]
pub trait NextOrPending {
    type Output;

    async fn next_or_pending(&mut self) -> Self::Output;

    async fn ok(&mut self) -> anyhow::Result<Self::Output>;
}

#[async_trait::async_trait]
impl<S> NextOrPending for S
where
    S: futures::Stream + Unpin + Send,
    S::Item: Send,
{
    type Output = S::Item;

    /// Waits for the next item in a stream. If the stream is closed while
    /// waiting, returns an error.  Useful when expecting a stream to progress.
    async fn ok(&mut self) -> anyhow::Result<Self::Output> {
        self.next()
            .await
            .map_or_else(|| Err(format_err!("Stream was unexpectedly closed")), Ok)
    }

    /// Waits for the next item in a stream. If the stream is closed while
    /// waiting the future will be pending forever. This is useful in cases
    /// where the future will be cancelled by shutdown logic anyway and handling
    /// each place where a stream may terminate would be too much trouble.
    async fn next_or_pending(&mut self) -> Self::Output {
        if let Some(item) = self.next().await {
            item
        } else {
            debug!(target: LOG_CORE, "Stream ended in next_or_pending, pending forever to avoid throwing an error on shutdown");
            std::future::pending().await
        }
    }
}

// TODO: make fully RFC1738 conformant
/// Wrapper for `Url` that only prints the scheme, domain, port and path portion
/// of a `Url` in its `Display` implementation.
///
/// This is useful to hide private
/// information like user names and passwords in logs or UIs.
///
/// The output is not fully RFC1738 conformant but good enough for our current
/// purposes.
#[derive(Hash, Clone, Serialize, Deserialize, Eq, PartialEq, Ord, PartialOrd)]
// nosemgrep: ban-raw-url
pub struct SafeUrl(Url);

crate::consensus_key!(SafeUrl);

impl picomint_encoding::Encodable for SafeUrl {
    fn consensus_encode<W: std::io::Write>(&self, w: &mut W) -> std::io::Result<()> {
        self.to_string().consensus_encode(w)
    }
}

impl picomint_encoding::Decodable for SafeUrl {
    fn consensus_decode<R: std::io::Read>(r: &mut R) -> std::io::Result<Self> {
        String::consensus_decode(r)?.parse().map_err(|e: url::ParseError| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, format!("invalid SafeUrl: {e}"))
        })
    }
}

impl SafeUrl {
    pub fn parse(url_str: &str) -> Result<Self, ParseError> {
        Url::parse(url_str).map(SafeUrl)
    }

    /// Warning: This removes the safety.
    // nosemgrep: ban-raw-url
    pub fn to_unsafe(self) -> Url {
        self.0
    }

    #[allow(clippy::result_unit_err)] // just copying `url`'s API here
    pub fn set_username(&mut self, username: &str) -> Result<(), ()> {
        self.0.set_username(username)
    }

    #[allow(clippy::result_unit_err)] // just copying `url`'s API here
    pub fn set_password(&mut self, password: Option<&str>) -> Result<(), ()> {
        self.0.set_password(password)
    }

    #[allow(clippy::result_unit_err)] // just copying `url`'s API here
    pub fn without_auth(&self) -> Result<Self, ()> {
        let mut url = self.clone();

        url.set_username("").and_then(|()| url.set_password(None))?;

        Ok(url)
    }

    pub fn host(&self) -> Option<Host<&str>> {
        self.0.host()
    }
    pub fn host_str(&self) -> Option<&str> {
        self.0.host_str()
    }
    pub fn scheme(&self) -> &str {
        self.0.scheme()
    }
    pub fn port(&self) -> Option<u16> {
        self.0.port()
    }
    pub fn port_or_known_default(&self) -> Option<u16> {
        self.0.port_or_known_default()
    }
    pub fn path(&self) -> &str {
        self.0.path()
    }
    /// Warning: This will expose username & password if present.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
    pub fn username(&self) -> &str {
        self.0.username()
    }
    pub fn password(&self) -> Option<&str> {
        self.0.password()
    }
    pub fn join(&self, input: &str) -> Result<Self, ParseError> {
        self.0.join(input).map(SafeUrl)
    }

    pub fn fragment(&self) -> Option<&str> {
        self.0.fragment()
    }

    pub fn set_fragment(&mut self, arg: Option<&str>) {
        self.0.set_fragment(arg);
    }
}

static SHOW_SECRETS: LazyLock<bool> = LazyLock::new(|| {
    let enable = is_env_var_set(DEBUG_SHOW_SECRETS_ENV);

    if enable {
        warn!(target: LOG_CORE, "{} enabled. Please don't use in production.", DEBUG_SHOW_SECRETS_ENV);
    }

    enable
});

impl Display for SafeUrl {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}://", self.0.scheme())?;

        if !self.0.username().is_empty() {
            let show_secrets = *SHOW_SECRETS;
            if show_secrets {
                write!(f, "{}", self.0.username())?;
            } else {
                write!(f, "REDACTEDUSER")?;
            }

            if self.0.password().is_some() {
                if show_secrets {
                    write!(
                        f,
                        ":{}",
                        self.0.password().expect("Just checked it's checked")
                    )?;
                } else {
                    write!(f, ":REDACTEDPASS")?;
                }
            }

            write!(f, "@")?;
        }

        if let Some(host) = self.0.host_str() {
            write!(f, "{host}")?;
        }

        if let Some(port) = self.0.port() {
            write!(f, ":{port}")?;
        }

        write!(f, "{}", self.0.path())?;

        Ok(())
    }
}

impl Debug for SafeUrl {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "SafeUrl(")?;
        Display::fmt(self, f)?;
        write!(f, ")")?;
        Ok(())
    }
}

impl From<Url> for SafeUrl {
    fn from(u: Url) -> Self {
        Self(u)
    }
}

impl FromStr for SafeUrl {
    type Err = ParseError;

    #[inline]
    fn from_str(input: &str) -> Result<Self, ParseError> {
        Self::parse(input)
    }
}

/// Write out a new file (like [`std::fs::write`] but fails if file already
/// exists)
pub fn write_new<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    let mut file = fs::File::options()
        .write(true)
        .create_new(true)
        .open(path)?;
    file.write_all(contents.as_ref())?;
    file.sync_all()?;
    Ok(())
}
pub fn write_overwrite<P: AsRef<Path>, C: AsRef<[u8]>>(path: P, contents: C) -> io::Result<()> {
    fs::File::options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)?
        .write_all(contents.as_ref())
}
pub async fn write_overwrite_async<P: AsRef<Path>, C: AsRef<[u8]>>(
    path: P,
    contents: C,
) -> io::Result<()> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?
        .write_all(contents.as_ref())
        .await
}
pub async fn write_new_async<P: AsRef<Path>, C: AsRef<[u8]>>(
    path: P,
    contents: C,
) -> io::Result<()> {
    tokio::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .await?
        .write_all(contents.as_ref())
        .await
}

/// Run the supplied closure `op_fn` until it succeeds. Frequency and number of
/// retries is determined by the specified strategy.
///
/// ```
/// use std::time::Duration;
///
/// use picomint_core::util::{backoff_util, retry};
/// # tokio_test::block_on(async {
/// retry(
///     "Gateway balance after swap".to_string(),
///     backoff_util::background_backoff(),
///     || async {
///         // Fallible network calls …
///         Ok(())
///     },
/// )
/// .await
/// .expect("never fails");
/// # });
/// ```
///
/// # Returns
///
/// - If the closure runs successfully, the result is immediately returned
/// - If the closure did not run successfully for `max_attempts` times, the
///   error of the closure is returned
pub async fn retry<F, Fut, T>(
    op_name: impl Into<String>,
    strategy: impl backoff_util::Backoff,
    op_fn: F,
) -> Result<T, anyhow::Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<T, anyhow::Error>>,
{
    let mut strategy = strategy;
    let op_name = op_name.into();
    let mut attempts: u64 = 0;
    loop {
        attempts += 1;
        match op_fn().await {
            Ok(result) => return Ok(result),
            Err(err) => {
                if let Some(interval) = strategy.next() {
                    // run closure op_fn again
                    debug!(
                        target: LOG_CORE,
                        err = %format_args!("{err:#}"),
                        %attempts,
                        interval = interval.as_secs(),
                        "{} failed, retrying",
                        op_name,
                    );
                    runtime::sleep(interval).await;
                } else {
                    warn!(
                        target: LOG_CORE,
                        err = %format_args!("{err:#}"),
                        %attempts,
                        "{} failed",
                        op_name,
                    );
                    return Err(err);
                }
            }
        }
    }
}

/// Computes the median from a slice of sorted `u64`s
pub fn get_median(vals: &[u64]) -> Option<u64> {
    if vals.is_empty() {
        return None;
    }
    let len = vals.len();
    let mid = len / 2;

    if len.is_multiple_of(2) {
        Some(u64::midpoint(vals[mid - 1], vals[mid]))
    } else {
        Some(vals[mid])
    }
}

/// Computes the average of the given `u64` slice.
pub fn get_average(vals: &[u64]) -> Option<u64> {
    if vals.is_empty() {
        return None;
    }

    let sum: u64 = vals.iter().sum();
    Some(sum / vals.len() as u64)
}

#[cfg(test)]
mod tests;
