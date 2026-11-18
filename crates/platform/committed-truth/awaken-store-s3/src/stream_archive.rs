//! Completed response indexes leave the hot head in the same CAS as their step.
use crate::{
    Error, Result, SessionStreams,
    model::{Head, StreamLog},
};
use std::collections::BTreeMap;
type Archive = BTreeMap<String, StreamLog>;

impl SessionStreams {
    async fn cold_logs(&self, head: &Head) -> Result<Archive> {
        match &head.stream_archive {
            Some(reference) => {
                self.session
                    .inner
                    .read_json(&self.session.key, reference)
                    .await
            }
            None => Ok(BTreeMap::new()),
        }
    }
    pub(crate) async fn stream_log_at(
        &self,
        head: &Head,
        identity: &str,
    ) -> Result<Option<StreamLog>> {
        if let Some(log) = head.frames.get(identity) {
            return Ok(Some(log.clone()));
        }
        Ok(self.cold_logs(head).await?.remove(identity))
    }
    pub(crate) async fn stream_logs_at(&self, head: &Head) -> Result<Archive> {
        let mut logs = self.cold_logs(head).await?;
        for (identity, log) in &head.frames {
            if logs.insert(identity.clone(), log.clone()).is_some() {
                return Err(Error::Corrupt(
                    "stream exists in both hot and archived indexes".into(),
                ));
            }
        }
        for (key, log) in &logs {
            if log.next_frame == 0 {
                continue;
            }
            let identity = log.identity.as_ref().ok_or_else(|| {
                Error::Invalid(
                    "legacy stream identity unavailable; read its original stream cursor".into(),
                )
            })?;
            if crate::streams::stream_key(identity) != *key || identity.next_frame != 0 {
                return Err(Error::Corrupt("stream index identity mismatch".into()));
            }
            let last_sequence = log
                .tail
                .last()
                .map(|frame| frame.sequence)
                .or_else(|| log.frontier.last().map(|index| index.last_sequence))
                .unwrap_or(0);
            if last_sequence > head.stream_sequence {
                return Err(Error::Corrupt(
                    "stream frame exceeds published ingest watermark".into(),
                ));
            }
        }
        Ok(logs)
    }
    pub(crate) async fn archive_step(
        &self,
        head: &mut Head,
        run: &str,
        ordinal: u64,
    ) -> Result<()> {
        let completed = head
            .frames
            .iter()
            .filter_map(|(key, log)| {
                log.identity
                    .as_ref()
                    .filter(|identity| identity.run == run && identity.step <= ordinal)
                    .map(|_| key.clone())
            })
            .collect::<Vec<_>>();
        if completed.is_empty() {
            return Ok(());
        }
        let mut archive = self.cold_logs(head).await?;
        for key in completed {
            let mut log = head
                .frames
                .remove(&key)
                .ok_or_else(|| Error::Corrupt("stream retirement lost its key".into()))?;
            self.seal_frame_index(head.gc_generation, &mut log).await?;
            if archive.insert(key, log).is_some() {
                return Err(Error::Corrupt(
                    "completed stream identity already archived".into(),
                ));
            }
        }
        head.stream_archive = Some(
            self.session
                .inner
                .put_json(&self.session.key, head.gc_generation, "indexes", &archive)
                .await?,
        );
        Ok(())
    }
}
