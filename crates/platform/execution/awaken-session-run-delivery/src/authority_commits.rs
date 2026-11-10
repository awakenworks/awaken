// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Forwards committed-history reads through the selected Runtime authority.
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
    fn shared_recovery_source(
        &self,
    ) -> Option<Arc<dyn awaken_agent_contract::thread::read::recovery::RunRecoverySource>> {
        self.0.shared_recovery_source()
    }

    async fn transcript_seek_page(
        &self,
        session: &str,
        request: &awaken_agent_contract::thread::read::transcript::TranscriptSeekRequest,
    ) -> Result<awaken_agent_contract::thread::read::transcript::TranscriptSeekPage, RunError> {
        self.0.transcript_seek_page(session, request).await
    }

    async fn archived_run_activation(
        &self,
        session: &str,
        thread: &awaken_agent_contract::ThreadId,
        run: &awaken_agent_contract::RunId,
    ) -> Result<Option<awaken_dispatch_contract::run_dispatch::ArchivedRunActivation>, RunError>
    {
        self.0.archived_run_activation(session, thread, run).await
    }

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
        session: &str,
        request: &TranscriptPageRequest,
    ) -> Result<TranscriptPage, RunError> {
        self.0.read_transcript_page(session, request).await
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
