/// A token proving the API call was authenticated
///
/// Api handlers are encouraged to take it as an argument to avoid sensitive
/// guardian-only logic being accidentally unauthenticated.
pub struct GuardianAuthToken {
    _marker: (), // private field just to make creating it outside impossible
}

impl GuardianAuthToken {
    /// Creates a token without verifying authentication.
    ///
    /// # Safety
    /// The caller must ensure that authentication has already been verified
    /// through other means before calling this function.
    pub fn new_unchecked() -> Self {
        Self { _marker: () }
    }
}
