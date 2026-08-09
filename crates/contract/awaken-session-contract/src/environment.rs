//! Durable identity of the execution environment bound to a Session.
//!
//! Rebuildable process capabilities such as a container Hand belong to the
//! Runtime Host and are deliberately absent from this durable aggregate state.

#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
pub enum SessionEnvironmentState {
    #[default]
    Unmaterialized,
    Resident {
        binding: String,
    },
}

impl SessionEnvironmentState {
    #[must_use]
    pub fn binding(&self) -> Option<&str> {
        match self {
            Self::Resident { binding } => Some(binding),
            Self::Unmaterialized => None,
        }
    }

    pub fn set_resident(&mut self, binding: impl Into<String>) {
        *self = Self::Resident {
            binding: binding.into(),
        };
    }
}
