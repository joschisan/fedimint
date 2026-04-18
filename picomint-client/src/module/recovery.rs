use std::fmt::{self, Debug};

use picomint_encoding::{Decodable, Encodable};
use serde::{Deserialize, Serialize};

/// Progress of the recovery
///
/// This includes "magic" value: if `total` is `0` the progress is "not started
/// yet"/"empty"/"none"
#[derive(Debug, Copy, Clone, Encodable, Decodable, Serialize, Deserialize)]
pub struct RecoveryProgress {
    pub complete: u32,
    pub total: u32,
}

impl RecoveryProgress {
    pub fn is_done(self) -> bool {
        !self.is_none() && self.total <= self.complete
    }

    pub fn none() -> RecoveryProgress {
        Self {
            complete: 0,
            total: 0,
        }
    }

    pub fn is_none(self) -> bool {
        self.total == 0
    }

    pub fn to_complete(self) -> RecoveryProgress {
        if self.is_none() {
            // Since we don't have a valid "total", we make up a 1 out of 1
            Self {
                complete: 1,
                total: 1,
            }
        } else {
            Self {
                complete: self.total,
                total: self.total,
            }
        }
    }
}

impl fmt::Display for RecoveryProgress {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{}/{}", self.complete, self.total))
    }
}
