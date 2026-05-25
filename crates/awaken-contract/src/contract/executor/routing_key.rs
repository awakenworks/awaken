use crate::contract::identity::RunIdentity;
use crate::registry_spec::StickyScope;

/// Stable routing identifiers for executors that need session affinity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferenceRoutingKey {
    /// Conversation/thread scope.
    pub thread_id: Option<String>,
    /// Single run scope.
    pub run_id: Option<String>,
    /// Fallback key for callers that do not have runtime identity.
    pub fallback: Option<String>,
    /// Stable id for retries belonging to one logical inference. Pool
    /// executors use this to keep streaming recovery from retrying a member
    /// that already failed within the same assistant response.
    pub logical_inference_id: Option<String>,
}

impl InferenceRoutingKey {
    pub fn from_run_identity(identity: &RunIdentity) -> Self {
        Self {
            thread_id: non_empty(identity.thread_id.as_str()),
            run_id: non_empty(identity.run_id.as_str()),
            fallback: None,
            logical_inference_id: None,
        }
    }

    pub fn thread(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: non_empty_owned(thread_id.into()),
            ..Default::default()
        }
    }

    pub fn fallback(key: impl Into<String>) -> Self {
        Self {
            fallback: non_empty_owned(key.into()),
            ..Default::default()
        }
    }

    pub fn for_scope(&self, scope: StickyScope) -> Option<String> {
        match scope {
            StickyScope::Thread => self.thread_id.clone(),
            StickyScope::Run => self.run_id.clone(),
        }
        .or_else(|| self.fallback.clone())
    }
}

fn non_empty(value: &str) -> Option<String> {
    non_empty_owned(value.to_string())
}

fn non_empty_owned(value: String) -> Option<String> {
    (!value.trim().is_empty()).then_some(value)
}
