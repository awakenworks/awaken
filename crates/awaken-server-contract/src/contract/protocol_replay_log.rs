pub use awaken_runtime_contract::contract::protocol_replay_log::*;

use std::sync::Arc;

use async_trait::async_trait;

use crate::contract::scope::{ScopeId, scoped_key, unscoped_key};

#[derive(Clone)]
pub struct ScopedProtocolReplayLog {
    inner: Arc<dyn ProtocolReplayLog>,
    scope_id: ScopeId,
}

impl ScopedProtocolReplayLog {
    pub fn new(inner: Arc<dyn ProtocolReplayLog>, scope_id: ScopeId) -> Self {
        Self { inner, scope_id }
    }

    pub fn scope_id(&self) -> &ScopeId {
        &self.scope_id
    }

    pub fn inner(&self) -> &dyn ProtocolReplayLog {
        self.inner.as_ref()
    }

    fn scoped(&self, value: &str) -> String {
        scoped_key(&self.scope_id, value)
    }

    fn unscoped<'a>(&self, value: &'a str) -> Option<&'a str> {
        unscoped_key(&self.scope_id, value)
    }

    fn encode_draft(&self, mut draft: ProtocolReplayDraft) -> ProtocolReplayDraft {
        draft.stream_id = self.scoped(&draft.stream_id);
        draft
    }

    fn encode_stream(&self, mut stream: ProtocolStreamKey) -> ProtocolStreamKey {
        stream.stream_id = self.scoped(&stream.stream_id);
        stream
    }

    fn decode_record(&self, mut record: ProtocolReplayRecord) -> Option<ProtocolReplayRecord> {
        record.stream_id = self.unscoped(&record.stream_id)?.to_string();
        Some(record)
    }
}

#[async_trait]
impl ProtocolReplayWriter for ScopedProtocolReplayLog {
    async fn append_replay(
        &self,
        draft: ProtocolReplayDraft,
    ) -> Result<ProtocolReplayAppendResult, ProtocolReplayError> {
        let result = self.inner.append_replay(self.encode_draft(draft)).await?;
        let record = self.decode_record(result.record).ok_or_else(|| {
            ProtocolReplayError::Integrity(
                "scoped replay log returned a record outside its scope".into(),
            )
        })?;
        Ok(ProtocolReplayAppendResult { record })
    }
}

#[async_trait]
impl ProtocolReplayReader for ScopedProtocolReplayLog {
    async fn list_replay(
        &self,
        stream: ProtocolStreamKey,
        from: Option<ProtocolReplayCursor>,
        limit: usize,
    ) -> Result<ProtocolReplayPage, ProtocolReplayError> {
        let mut page = self
            .inner
            .list_replay(self.encode_stream(stream), from, limit)
            .await?;
        page.records = page
            .records
            .into_iter()
            .filter_map(|record| self.decode_record(record))
            .collect();
        Ok(page)
    }
}

#[async_trait]
impl ProtocolReplayLookup for ScopedProtocolReplayLog {
    async fn load_replay(
        &self,
        protocol_replay_id: &ProtocolReplayId,
    ) -> Result<Option<ProtocolReplayRecord>, ProtocolReplayError> {
        Ok(self
            .inner
            .load_replay(protocol_replay_id)
            .await?
            .and_then(|record| self.decode_record(record)))
    }
}
