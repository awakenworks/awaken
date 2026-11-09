//! [`AuthorityCommits`]: committed Session reads straight from the process's
//! Runtime authority, for a process that holds no resident execution context.

use std::sync::Arc;

use awaken_agent_contract::thread::read::transcript::{TranscriptPage, TranscriptPageRequest};
use awaken_dispatch_erasure::{RuntimeAuthority, RuntimeAuthorityError};
use awaken_runtime_contract::runs::RunError;

use crate::{SessionCommit, SessionCommitSource};

/// A Runtime authority failure as the neutral Run error.
#[must_use]
pub fn runtime_authority_error(error: RuntimeAuthorityError) -> RunError {
    match error {
        RuntimeAuthorityError::Unavailable(message) => RunError::unavailable(message),
        RuntimeAuthorityError::Corrupt(message) => {
            RunError::classified("runtime_authority_corrupt", message)
        }
        RuntimeAuthorityError::Misconfigured(message) => {
            RunError::classified("runtime_authority_misconfigured", message)
        }
    }
}

/// Every Session's committed truth, opened from the Runtime authority. The
/// Coordinator's committed reads: it executes no Run, so no resident context
/// can hold a fresher commit boundary.
pub struct AuthorityCommits(Arc<dyn RuntimeAuthority>);

impl AuthorityCommits {
    #[must_use]
    pub fn new(authority: Arc<dyn RuntimeAuthority>) -> Self {
        Self(authority)
    }
}

#[async_trait::async_trait]
impl SessionCommitSource for AuthorityCommits {
    async fn open_for_read(&self, session: &str) -> Result<Arc<SessionCommit>, RunError> {
        Ok(Arc::new(SessionCommit::Local(
            self.0
                .open_commit(session)
                .await
                .map_err(runtime_authority_error)?,
        )))
    }

    async fn durable_thread_exists(&self, thread: &str) -> Result<bool, RunError> {
        self.0
            .durable_thread_exists(thread)
            .await
            .map_err(runtime_authority_error)
    }

    async fn read_transcript_page(
        &self,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, RunError> {
        self.0
            .read_transcript_page(request)
            .await
            .map_err(runtime_authority_error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_runtime_contract::runs::RunErrorKind;

    #[test]
    fn runtime_authority_failures_keep_one_run_class() {
        // Causes: unavailable/corrupt/misconfigured original authority error.
        // Effects: the same neutral fault/code plus the unchanged diagnostic.
        // The three rows cover the closed source variants; no text selects policy.
        let rows = [
            (
                RuntimeAuthorityError::Unavailable("x".into()),
                RunErrorKind::Unavailable,
                "unavailable",
            ),
            (
                RuntimeAuthorityError::Corrupt("x".into()),
                RunErrorKind::Internal,
                "runtime_authority_corrupt",
            ),
            (
                RuntimeAuthorityError::Misconfigured("x".into()),
                RunErrorKind::Internal,
                "runtime_authority_misconfigured",
            ),
        ];
        for (cause, kind, code) in rows {
            let error = runtime_authority_error(cause);
            assert_eq!((error.kind, error.code.as_str()), (kind, code));
            assert_eq!(
                error.message, "x",
                "the original authority diagnostic survives"
            );
        }
    }
}
