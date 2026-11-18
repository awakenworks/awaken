//! Ordered Run replay and live tailing use the same durable ingest cursor.
use crate::{
    Error, Result, SessionStreams, StreamBatch, StreamReadCursor, StreamReadPage,
    StreamSubscription,
};

impl SessionStreams {
    /// Read one bounded page. Supply the first page's watermark to freeze a history scan.
    /// Leaving it unset tails the latest committed stream head.
    pub async fn read_run_page(
        &self,
        cursor: &StreamReadCursor,
        limit: usize,
        through: Option<u64>,
    ) -> Result<StreamReadPage> {
        if cursor.key != self.session.key || cursor.run.is_empty() || limit > 4096 {
            return Err(Error::Invalid(
                "invalid stream page scope, Run or limit".into(),
            ));
        }
        let (head, _) = self.session.inner.load_head(&self.session.key).await?;
        let watermark = through.unwrap_or(head.stream_sequence);
        if cursor.after > watermark || watermark > head.stream_sequence {
            return Err(Error::Invalid(
                "stream cursor outside published watermark".into(),
            ));
        }
        if limit == 0 {
            return Ok(StreamReadPage {
                batches: Vec::new(),
                next_cursor: cursor.clone(),
                watermark,
            });
        }
        let mut frames = Vec::new();
        for log in self.stream_logs_at(&head).await?.values() {
            if log.identity.as_ref().is_some_and(|id| id.run == cursor.run) {
                frames.extend(
                    self.frames_after_sequence(log, cursor.after, watermark, limit + 1)
                        .await?,
                );
            }
        }
        frames.sort_by_key(|frame| frame.sequence);
        if frames
            .windows(2)
            .any(|pair| pair[0].sequence == pair[1].sequence)
        {
            return Err(Error::Corrupt("duplicate stream ingest sequence".into()));
        }
        let has_more = frames.len() > limit;
        frames.truncate(limit);
        let mut next_cursor = cursor.clone();
        next_cursor.after = if has_more {
            frames.last().map_or(cursor.after, |f| f.sequence)
        } else {
            watermark
        };
        let mut batches = Vec::with_capacity(frames.len());
        for frame in frames {
            if frame.cursor.run != cursor.run {
                return Err(Error::Corrupt("stream frame Run mismatch".into()));
            }
            batches.push(StreamBatch {
                cursor: frame.cursor,
                sequence: frame.sequence,
                content_type: frame.content_type,
                bytes: self
                    .session
                    .inner
                    .read_bytes(&self.session.key, &frame.object)
                    .await?,
            });
        }
        Ok(StreamReadPage {
            batches,
            next_cursor,
            watermark,
        })
    }

    /// Attach before replay; notifications are hints, including across processes.
    pub async fn subscribe_run(&self, cursor: StreamReadCursor) -> Result<StreamSubscription> {
        let receiver = self.session.inner.channel(&self.session.key)?.subscribe();
        self.read_run_page(&cursor, 0, None).await?;
        Ok(StreamSubscription { cursor, receiver })
    }
    pub async fn next_stream_batches(
        &self,
        subscription: &mut StreamSubscription,
        limit: usize,
    ) -> Result<StreamReadPage> {
        loop {
            let page = self
                .read_run_page(&subscription.cursor, limit, None)
                .await?;
            subscription.cursor = page.next_cursor.clone();
            if !page.batches.is_empty() || limit == 0 {
                return Ok(page);
            }
            let _ = tokio::time::timeout(
                std::time::Duration::from_millis(250),
                subscription.receiver.recv(),
            )
            .await;
        }
    }
}
