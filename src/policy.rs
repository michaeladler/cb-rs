use std::path::Path;

use rustix::io::{Errno, Result as IoResult};

use crate::mover;

/// What to do when the destination already exists. Default is to leave it
/// alone and report; overwriting a file on a paste the user did not aim at is
/// not recoverable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Skip,
    Replace,
    Ask,
}

impl Policy {
    /// Decide whether the copy may proceed. The answer is about what the
    /// destination looked like when this was called, not about what it looks
    /// like when the write lands: a destination that appears or changes while
    /// the copy is staged needs its own approval before replacement.
    pub fn resolve(self, dst: &Path) -> IoResult<bool> {
        Ok(match self {
            Self::Skip => false,
            Self::Replace => true,
            Self::Ask => mover::prompt_replace(dst.file_name().unwrap_or_default()),
        })
    }
}

impl std::str::FromStr for Policy {
    type Err = Errno;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "skip" => Ok(Self::Skip),
            "replace" => Ok(Self::Replace),
            "ask" => Ok(Self::Ask),
            _ => Err(Errno::INVAL),
        }
    }
}
