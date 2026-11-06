//! Worker-reported retention of the original Session sources a Worker holds.
//!
//! A draining Worker observes the Sandbox sources it holds through its own
//! providers and reports one fact per Session subject the Coordinator asked
//! about. The Coordinator decides retirement from these facts and holds no
//! provider of its own; a fact counts only for the exact subject it names.

use serde::{Deserialize, Serialize};

/// One retention fact a Worker observed for one original Session source.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSourceRetention {
    pub session_id: String,
    /// The Coordinator's digest of the exact observation subject; a fact for
    /// another digest says nothing about the current subject.
    pub subject: String,
    /// The Worker positively observed the original source retained.
    pub retained: bool,
}
