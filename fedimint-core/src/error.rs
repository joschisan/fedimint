//! The code an error crosses an admin socket with.
//!
//! Every error a daemon hands back over its admin socket is a `thiserror`
//! enum deriving [`ErrorCode`]: the variant names the code, the `#[error]`
//! message explains it. The daemon puts both in the JSON error body, and
//! the CLI's `--help` lists every code a command fails with, from the same
//! enum, so the list cannot drift from what the daemon returns. The derive
//! is [`fedimint_derive::ErrorCode`], re-exported here.

pub use fedimint_derive::ErrorCode;

/// An error enum whose variants are stable codes.
///
/// A variant marked `#[error(transparent)]` wraps another such enum and
/// contributes that enum's codes instead of its own name, so an operation's
/// enum composes a helper's without restating it.
pub trait ErrorCode {
    /// Every code the enum can carry as `(code, message)`: the variant name
    /// in snake_case and its `#[error]` message as written, format
    /// placeholders included; a transparent variant's inner codes in its
    /// place.
    fn codes() -> Vec<(&'static str, &'static str)>;

    /// This value's code.
    fn code(&self) -> &'static str;
}
