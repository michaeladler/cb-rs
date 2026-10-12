use std::io::IsTerminal;
use std::path::Path;

use rustix::io::{Errno, Result as IoResult};

use crate::mover;

/// What to do when the destination already exists. Without an explicit
/// `--on-conflict` this is [`Policy::default_for_terminal`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Policy {
    Skip,
    Replace,
    Ask,
}

impl Policy {
    /// What a paste does with an existing destination when the user named no
    /// policy.
    ///
    /// Ask when stdin is a terminal. Skip otherwise, avoiding blocked scripts.
    pub fn default_for_terminal() -> Self {
        Self::default_for(std::io::stdin().is_terminal())
    }

    pub fn default_for(terminal: bool) -> Self {
        if terminal { Self::Ask } else { Self::Skip }
    }

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_terminal_stdin_asks_and_a_piped_stdin_skips() {
        assert_eq!(Policy::default_for(true), Policy::Ask);
        assert_eq!(Policy::default_for(false), Policy::Skip);
    }

    #[test]
    fn the_policy_parses_from_its_flag_spelling() {
        for (text, policy) in [
            ("skip", Policy::Skip),
            ("replace", Policy::Replace),
            ("ask", Policy::Ask),
        ] {
            assert_eq!(text.parse(), Ok(policy));
        }
        assert!("bogus".parse::<Policy>().is_err());
    }

    /// Every policy but `ask` decides without looking at the destination, so
    /// these hold with no terminal and no way to answer.
    #[test]
    fn skip_and_replace_need_no_terminal() {
        assert!(!Policy::Skip.resolve(Path::new("/nowhere/at/all")).unwrap());
        assert!(
            Policy::Replace
                .resolve(Path::new("/nowhere/at/all"))
                .unwrap()
        );
    }
}
