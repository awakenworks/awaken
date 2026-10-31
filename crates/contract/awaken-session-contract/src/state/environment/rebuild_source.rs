// @formal surface requirement=session.lifecycle.session_state
// @formal reason: Static owner: the realization's observation of the source a Rebuild replaces, carried
// @formal reason: on the Rebuild intent. Dynamic path: realization derives it from exact provider and
// @formal reason: process-end evidence; only the aggregate's transition function reads it, as the
// @formal reason: source_closed and source_access facts. It grants no physical or lease authority.
//! What the realization observed about the source a Rebuild replaces.
//!
//! A Rebuild replaces a Resident binding whose substrate died. The aggregate
//! may admit it only when the old source is closed (nothing can still write
//! to it) and the Session's work survives somewhere to rebuild from. Both are
//! physical observations, so the realization derives them and carries them on
//! the Rebuild intent; the transition function reads them as two facts.
//!
//! The evidence is an admission precondition, not an operation coordinate:
//! it is not part of the intent's effect identity, so a retry that observes
//! the same source again names the same operation.

/// What a closed source can be rebuilt from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RebuildSourceAccess {
    /// The source's files outlive its processes (a retained volume or the
    /// Worker's own filesystem), so the provider rebuilds over them.
    RetainedVolume,
    /// The Session's workspace is a Repository input the rebuild checks out
    /// again.
    Repository,
}

/// The realization's evidence for one Rebuild admission.
///
/// `closed`: the provider observed the exact bound handle Terminal or
/// Unavailable and, for a source bound to a Worker process, that process's end
/// is recorded. `access`: what the rebuild can restore the Session's work from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RebuildSourceEvidence {
    closed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    access: Option<RebuildSourceAccess>,
}

impl RebuildSourceEvidence {
    #[must_use]
    pub const fn new(closed: bool, access: Option<RebuildSourceAccess>) -> Self {
        Self { closed, access }
    }

    /// The `source_closed` fact.
    #[must_use]
    pub const fn source_closed(&self) -> bool {
        self.closed
    }

    /// What the rebuild can restore from; `Some` is the `source_access` fact.
    #[must_use]
    pub const fn source_access(&self) -> Option<RebuildSourceAccess> {
        self.access
    }
}
