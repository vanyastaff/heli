//! A run's priority (docs/host-pool.md, "Priorities"): how large its share of
//! the host pool is beside the other runs'. A person waiting at a terminal
//! runs `interactive`, agents run `normal`, and long work nobody waits for
//! (mutation testing, full sweeps) runs `background`.
//!
//! The client chooses it ([`choose`]): heli's `--priority` flag, else
//! [`VAR`], else what the tool's profile says (cargo-mutants runs in the
//! background), else `interactive` when stdin and stdout are terminals and
//! no agent marker ([`AGENT_MARKERS`]) is set, else `normal`.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// The variable that sets the priority of every run started under it.
pub const VAR: &str = "HELI_PRIORITY";

/// Variables that say an agent or CI runs heli, not a person, even on a
/// terminal.
pub const AGENT_MARKERS: &[&str] = &["CLAUDECODE", "CI"];

/// How much of the host pool a run gets beside the others.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    /// A person waits at a terminal.
    Interactive,
    /// An agent's run; what an older client's runs are.
    #[default]
    Normal,
    /// Long and not urgent.
    Background,
}

impl Priority {
    /// The run's weight in the broker's shares: a share is the capacity
    /// times the run's weight over the sum of the active runs' weights.
    pub fn weight(self) -> f64 {
        match self {
            Self::Interactive => 4.0,
            Self::Normal => 1.0,
            Self::Background => 0.25,
        }
    }

    /// The name the flag, [`VAR`] and `status` use.
    pub fn name(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Normal => "normal",
            Self::Background => "background",
        }
    }

    pub fn is_normal(&self) -> bool {
        *self == Self::Normal
    }
}

impl fmt::Display for Priority {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Priority {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text {
            "interactive" => Ok(Self::Interactive),
            "normal" => Ok(Self::Normal),
            "background" => Ok(Self::Background),
            _ => Err(format!(
                "unknown priority {text:?}; expected interactive, normal or background"
            )),
        }
    }
}

/// A priority as a message reads it: one this build does not know (a newer
/// side's) reads as normal.
pub fn known<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Priority, D::Error> {
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    Ok(value
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default())
}

/// The priority of a run: `flag`, else [`VAR`] (`var`, its value), else
/// `profile` (the tool profile's default), else interactive when a person
/// is at the `terminal` and no agent marker is set (`agent`), else normal.
/// The error says why [`VAR`] is not a priority.
pub fn choose(
    flag: Option<Priority>,
    var: Option<&str>,
    profile: Option<Priority>,
    terminal: bool,
    agent: bool,
) -> Result<Priority, String> {
    if let Some(priority) = flag {
        return Ok(priority);
    }
    if let Some(value) = var.filter(|value| !value.is_empty()) {
        return value
            .parse()
            .map_err(|error| format!("{VAR}={value}: {error}"));
    }
    if let Some(priority) = profile {
        return Ok(priority);
    }
    Ok(if terminal && !agent {
        Priority::Interactive
    } else {
        Priority::Normal
    })
}

#[cfg(test)]
mod tests {
    use super::{Priority, choose};

    #[test]
    fn the_flag_then_the_variable_then_the_profile_then_the_terminal() {
        use Priority::*;
        let flag = Some(Background);
        assert_eq!(
            choose(flag, Some("interactive"), None, true, false),
            Ok(Background)
        );
        assert_eq!(
            choose(None, Some("background"), Some(Normal), true, false),
            Ok(Background)
        );
        assert_eq!(
            choose(None, Some(""), Some(Background), true, false),
            Ok(Background)
        );
        assert_eq!(choose(None, None, None, true, false), Ok(Interactive));
        assert_eq!(choose(None, None, None, true, true), Ok(Normal));
        assert_eq!(choose(None, None, None, false, false), Ok(Normal));
        assert!(choose(None, Some("urgent"), None, true, false).is_err());
    }

    #[test]
    fn an_unknown_or_absent_priority_reads_as_normal() {
        #[derive(serde::Deserialize)]
        struct Message {
            #[serde(default, deserialize_with = "super::known")]
            priority: Priority,
        }
        let read = |json| {
            serde_json::from_str::<Message>(json)
                .expect("message")
                .priority
        };
        assert_eq!(read(r#"{}"#), Priority::Normal);
        assert_eq!(read(r#"{"priority":"urgent"}"#), Priority::Normal);
        assert_eq!(read(r#"{"priority":"background"}"#), Priority::Background);
    }
}
