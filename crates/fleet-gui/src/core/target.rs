use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Which fleetd the GUI talks to.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum Target {
    Local,
    /// `dest` is what ssh takes: `user@host` or `host`.
    Ssh {
        dest: String,
    },
}

#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("invalid target {0:?}: expected `local` or `ssh://[user@]host`")]
pub struct ParseTargetError(String);

impl FromStr for Target {
    type Err = ParseTargetError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "local" {
            return Ok(Target::Local);
        }
        match s.strip_prefix("ssh://") {
            Some(dest) if !dest.is_empty() => Ok(Target::Ssh {
                dest: dest.to_owned(),
            }),
            _ => Err(ParseTargetError(s.to_owned())),
        }
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Target::Local => f.write_str("local"),
            Target::Ssh { dest } => write!(f, "ssh://{dest}"),
        }
    }
}

impl From<Target> for String {
    fn from(t: Target) -> String {
        t.to_string()
    }
}

impl TryFrom<String> for Target {
    type Error = ParseTargetError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_parses_and_formats() {
        for s in ["local", "ssh://me@box", "ssh://box"] {
            assert_eq!(s.parse::<Target>().unwrap().to_string(), s);
        }
        assert!("ftp://x".parse::<Target>().is_err());
        assert!("ssh://".parse::<Target>().is_err());
    }
}
