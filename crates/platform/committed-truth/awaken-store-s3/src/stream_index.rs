//! Radix-64 indexes retain bounded tails and prune by local ordinal or ingest sequence.
use crate::{
    Error, Result, SessionStreams,
    model::{StreamFrame, StreamIndex, StreamIndexRef, StreamLog},
};
use futures::future::BoxFuture;
const FANOUT: usize = 64;

fn sequence_bounds(frames: &[StreamFrame]) -> (u64, u64) {
    if frames.iter().any(|frame| frame.sequence == 0) {
        return (0, 0);
    }
    (
        frames.first().map_or(0, |f| f.sequence),
        frames.last().map_or(0, |f| f.sequence),
    )
}
fn branch_sequence_bounds(children: &[StreamIndexRef]) -> (u64, u64) {
    if children
        .iter()
        .any(|child| child.first_sequence == 0 || child.last_sequence == 0)
    {
        return (0, 0);
    }
    (
        children.first().map_or(0, |c| c.first_sequence),
        children.last().map_or(0, |c| c.last_sequence),
    )
}
fn validate_node(index: &StreamIndexRef, node: &StreamIndex) -> Result<()> {
    if index.first >= index.end {
        return Err(Error::Corrupt("stream index bounds".into()));
    }
    let valid = match node {
        StreamIndex::Frames(frames) => {
            index.level == 0
                && !frames.is_empty()
                && frames.len() <= FANOUT
                && frames.len() as u64 == index.end - index.first
                && frames
                    .iter()
                    .enumerate()
                    .all(|(i, frame)| frame.cursor.next_frame == index.first + i as u64 + 1)
                && frames.windows(2).all(|pair| {
                    pair[0].sequence == 0
                        || pair[1].sequence == 0
                        || pair[0].sequence < pair[1].sequence
                })
                && (index.first_sequence == 0
                    || sequence_bounds(frames) == (index.first_sequence, index.last_sequence))
        }
        StreamIndex::Children(children) => {
            index.level > 0
                && children.len() == FANOUT
                && children.first().map(|c| c.first) == Some(index.first)
                && children.last().map(|c| c.end) == Some(index.end)
                && children.windows(2).all(|pair| {
                    pair[0].end == pair[1].first
                        && (pair[0].last_sequence == 0
                            || pair[1].first_sequence == 0
                            || pair[0].last_sequence < pair[1].first_sequence)
                })
                && children
                    .iter()
                    .all(|child| child.level.checked_add(1) == Some(index.level))
                && (index.first_sequence == 0
                    || branch_sequence_bounds(children)
                        == (index.first_sequence, index.last_sequence))
        }
    };
    if valid {
        Ok(())
    } else {
        Err(Error::Corrupt(
            "stream index node disagrees with its bounds".into(),
        ))
    }
}
impl SessionStreams {
    pub(crate) async fn seal_frame_index(
        &self,
        generation: u64,
        log: &mut StreamLog,
    ) -> Result<()> {
        if log.tail.is_empty() {
            return Ok(());
        }
        let frames = std::mem::take(&mut log.tail);
        let first = frames[0]
            .cursor
            .next_frame
            .checked_sub(1)
            .ok_or_else(|| Error::Corrupt("stream ordinal zero".into()))?;
        let end = frames
            .last()
            .ok_or_else(|| Error::Corrupt("empty stream tail".into()))?
            .cursor
            .next_frame;
        let (first_sequence, last_sequence) = sequence_bounds(&frames);
        let object = self
            .session
            .inner
            .put_json(
                &self.session.key,
                generation,
                "indexes",
                &StreamIndex::Frames(frames),
            )
            .await?;
        log.frontier.push(StreamIndexRef {
            object,
            first,
            end,
            level: 0,
            first_sequence,
            last_sequence,
        });
        Ok(())
    }
    pub(crate) async fn append_frame_index(
        &self,
        generation: u64,
        log: &mut StreamLog,
        frame: StreamFrame,
    ) -> Result<()> {
        if frame.cursor.next_frame
            != log
                .next_frame
                .checked_add(1)
                .ok_or_else(|| Error::Capacity("stream ordinal exhausted".into()))?
        {
            return Err(Error::Corrupt("stream append is not contiguous".into()));
        }
        if log.identity.is_none() {
            let mut identity = frame.cursor.clone();
            identity.next_frame = 0;
            log.identity = Some(identity);
        }
        log.next_frame = frame.cursor.next_frame;
        log.tail.push(frame);
        while log.tail.len() >= FANOUT {
            let frames = log.tail.drain(..FANOUT).collect::<Vec<_>>();
            let first = frames[0].cursor.next_frame - 1;
            let end = frames[FANOUT - 1].cursor.next_frame;
            let (first_sequence, last_sequence) = sequence_bounds(&frames);
            let object = self
                .session
                .inner
                .put_json(
                    &self.session.key,
                    generation,
                    "indexes",
                    &StreamIndex::Frames(frames),
                )
                .await?;
            log.frontier.push(StreamIndexRef {
                object,
                first,
                end,
                level: 0,
                first_sequence,
                last_sequence,
            });
            loop {
                let len = log.frontier.len();
                if len < FANOUT {
                    break;
                }
                let level = log.frontier[len - FANOUT].level;
                if !log.frontier[len - FANOUT..]
                    .iter()
                    .all(|child| child.level == level)
                {
                    break;
                }
                let parent_level = level
                    .checked_add(1)
                    .ok_or_else(|| Error::Capacity("stream index depth".into()))?;
                let children = log.frontier.split_off(len - FANOUT);
                let first = children[0].first;
                let end = children[FANOUT - 1].end;
                let (first_sequence, last_sequence) = branch_sequence_bounds(&children);
                let object = self
                    .session
                    .inner
                    .put_json(
                        &self.session.key,
                        generation,
                        "indexes",
                        &StreamIndex::Children(children),
                    )
                    .await?;
                log.frontier.push(StreamIndexRef {
                    object,
                    first,
                    end,
                    level: parent_level,
                    first_sequence,
                    last_sequence,
                });
            }
        }
        Ok(())
    }
    async fn index_node(&self, index: &StreamIndexRef) -> Result<StreamIndex> {
        let node = self
            .session
            .inner
            .read_json(&self.session.key, &index.object)
            .await?;
        validate_node(index, &node)?;
        Ok(node)
    }
    pub(crate) async fn frame_range(
        &self,
        log: &StreamLog,
        start: u64,
        end: u64,
    ) -> Result<Vec<StreamFrame>> {
        if start > end || end > log.next_frame {
            return Err(Error::Invalid(
                "stream range outside published watermark".into(),
            ));
        }
        let mut frames = Vec::new();
        for index in &log.frontier {
            if start < index.end && end > index.first {
                frames.extend(self.index_range(index, start, end, 0).await?);
            }
        }
        frames.extend(
            log.tail
                .iter()
                .filter(|frame| frame.cursor.next_frame > start && frame.cursor.next_frame <= end)
                .cloned(),
        );
        if frames.len() as u64 != end - start
            || frames
                .iter()
                .enumerate()
                .any(|(i, frame)| frame.cursor.next_frame != start + i as u64 + 1)
        {
            return Err(Error::Corrupt(
                "stream index contains a gap or overlap".into(),
            ));
        }
        Ok(frames)
    }
    fn index_range<'a>(
        &'a self,
        index: &'a StreamIndexRef,
        start: u64,
        end: u64,
        depth: usize,
    ) -> BoxFuture<'a, Result<Vec<StreamFrame>>> {
        Box::pin(async move {
            if depth > 32 {
                return Err(Error::Corrupt("stream index depth".into()));
            }
            match self.index_node(index).await? {
                StreamIndex::Frames(frames) => Ok(frames
                    .into_iter()
                    .filter(|frame| {
                        frame.cursor.next_frame > start && frame.cursor.next_frame <= end
                    })
                    .collect()),
                StreamIndex::Children(children) => {
                    let mut frames = Vec::new();
                    for child in &children {
                        if start < child.end && end > child.first {
                            frames.extend(self.index_range(child, start, end, depth + 1).await?);
                        }
                    }
                    Ok(frames)
                }
            }
        })
    }
    pub(crate) async fn frames_after_sequence(
        &self,
        log: &StreamLog,
        after: u64,
        through: u64,
        limit: usize,
    ) -> Result<Vec<StreamFrame>> {
        let mut frames = Vec::new();
        for index in &log.frontier {
            frames.extend(
                self.index_sequence_range(index, after, through, limit - frames.len(), 0)
                    .await?,
            );
            if frames.len() == limit {
                return Ok(frames);
            }
        }
        for frame in &log.tail {
            if frame.sequence == 0 {
                return Err(Error::Invalid("legacy frames have no cross-stream ingest order; read their original stream cursor".into()));
            }
            if frame.sequence > after && frame.sequence <= through {
                frames.push(frame.clone());
            }
        }
        Ok(frames)
    }
    fn index_sequence_range<'a>(
        &'a self,
        index: &'a StreamIndexRef,
        after: u64,
        through: u64,
        limit: usize,
        depth: usize,
    ) -> BoxFuture<'a, Result<Vec<StreamFrame>>> {
        Box::pin(async move {
            if depth > 32 {
                return Err(Error::Corrupt("stream index depth".into()));
            }
            if limit == 0 {
                return Ok(Vec::new());
            }
            if index.first_sequence > 0
                && (index.last_sequence <= after || index.first_sequence > through)
            {
                return Ok(Vec::new());
            }
            match self.index_node(index).await? {
                StreamIndex::Frames(frames) => {
                    if frames.iter().any(|frame| frame.sequence == 0) {
                        return Err(Error::Invalid("legacy frames have no cross-stream ingest order; read their original stream cursor".into()));
                    }
                    Ok(frames
                        .into_iter()
                        .filter(|frame| frame.sequence > after && frame.sequence <= through)
                        .take(limit)
                        .collect())
                }
                StreamIndex::Children(children) => {
                    let mut frames = Vec::new();
                    for child in &children {
                        frames.extend(
                            self.index_sequence_range(
                                child,
                                after,
                                through,
                                limit - frames.len(),
                                depth + 1,
                            )
                            .await?,
                        );
                        if frames.len() == limit {
                            break;
                        }
                    }
                    Ok(frames)
                }
            }
        })
    }
    pub(crate) async fn stream_roots(&self, log: &StreamLog) -> Result<Vec<String>> {
        let mut keys = log
            .tail
            .iter()
            .map(|frame| frame.object.key.clone())
            .collect::<Vec<_>>();
        let mut pending = log.frontier.clone();
        let mut seen = std::collections::BTreeSet::new();
        while let Some(index) = pending.pop() {
            if !seen.insert(index.object.key.clone()) {
                return Err(Error::Corrupt("cycle in stream index".into()));
            }
            keys.push(index.object.key.clone());
            match self.index_node(&index).await? {
                StreamIndex::Frames(frames) => {
                    keys.extend(frames.into_iter().map(|frame| frame.object.key))
                }
                StreamIndex::Children(children) => pending.extend(children),
            }
        }
        Ok(keys)
    }
}
