//! Narrow Coordinator composition ports exposed by the runtime Host.

use std::sync::Arc;

use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::stream::sink::Sink as StreamSink;
use awaken_agent_contract::thread::read::recovery::{
    RecoveryError, RunRecoverySnapshot, RunRecoverySource,
};
use awaken_run_ingress::CompletionSink;

use crate::{FileContentSource, SharedHost};

struct HostRunRecoverySource(Arc<SharedHost>);

#[async_trait::async_trait]
impl RunRecoverySource for HostRunRecoverySource {
    async fn recovery_snapshot(
        &self,
        thread_id: &awaken_agent_contract::agent::thread::Id,
        claimed_run_id: &RunId,
    ) -> Result<RunRecoverySnapshot, RecoveryError> {
        let context = self
            .0
            .ctx_for(&thread_id.0, None)
            .await
            .map_err(|error| RecoveryError::Rejected(error.to_string()))?;
        context
            .commit
            .recovery_snapshot(thread_id, claimed_run_id)
            .await
    }
}

impl SharedHost {
    pub fn worker_recovery_source(self: &Arc<Self>) -> Arc<dyn RunRecoverySource> {
        Arc::new(HostRunRecoverySource(self.clone()))
    }

    pub fn worker_completion_sink(&self) -> Arc<dyn CompletionSink> {
        self.completion.clone()
    }

    pub fn worker_stream_sink(&self) -> Arc<dyn StreamSink> {
        self.completion.clone()
    }

    pub fn worker_file_content_source(&self) -> Arc<dyn FileContentSource> {
        self.file_content_source.clone()
    }
}
