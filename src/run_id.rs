//! Run ids (DESIGN.md §5, "Run ids"): `r<number>`, unique for the life of
//! the state directory. The daemon gives them (`crate::daemon::registry`);
//! records, markers, the protocol and `cancel` name runs by them. On the
//! wire and on disk an id is its text, `r17`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// A run's id, `r<number>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(into = "String", try_from = "String")]
pub struct RunId(u64);

impl RunId {
    /// The id of run number `number`.
    pub fn new(number: u64) -> Self {
        Self(number)
    }

    /// Its number.
    pub fn number(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RunId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "r{}", self.0)
    }
}

/// `r<number>`, digits only.
impl FromStr for RunId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        text.strip_prefix('r')
            .filter(|digits| !digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
            .and_then(|digits| digits.parse().ok())
            .map(Self)
            .ok_or_else(|| format!("{text:?} is no run id: a run id is r<number>"))
    }
}

impl From<RunId> for String {
    fn from(id: RunId) -> Self {
        id.to_string()
    }
}

impl TryFrom<String> for RunId {
    type Error = String;

    fn try_from(text: String) -> Result<Self, String> {
        text.parse()
    }
}
