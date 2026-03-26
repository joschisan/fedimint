pub mod dashboard;
pub mod setup;

use serde::Deserialize;

pub(crate) const LOG_UI: &str = "fm::ui";

// Common route constants
pub const EXPLORER_IDX_ROUTE: &str = "/explorer";
pub const EXPLORER_ROUTE: &str = "/explorer/{session_idx}";
pub const DOWNLOAD_BACKUP_ROUTE: &str = "/download-backup";
pub const CHANGE_PASSWORD_ROUTE: &str = "/change-password";
pub const METRICS_ROUTE: &str = "/metrics";

#[derive(Debug, Deserialize)]
pub struct PasswordChangeInput {
    pub current_password: String,
    pub new_password: String,
    pub confirm_password: String,
}
