// @formal surface requirement=store.backends.schema_and_adapter_conformance
// @formal reason: Defines the Session state, claim and receipt records published by conditional head replacement.
use std::collections::BTreeMap;

use awaken_agent_contract::{
    agent::message::Message,
    stream::checkpoint::StreamCheckpoint,
    thread::{
        commit::operation::{CommitOperation, CommitReceipt},
        read::transcript::TranscriptSnapshotRef,
    },
};
use awaken_dispatch_contract::RunDispatch;
use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Trusted application code binds this key after resolving the caller's scope.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "SessionKeyWire")]
pub struct SessionKey {
    pub(crate) scope: String,
    pub(crate) session: String,
}
#[derive(Deserialize)]
struct SessionKeyWire {
    scope: String,
    session: String,
}
impl TryFrom<SessionKeyWire> for SessionKey {
    type Error = Error;
    fn try_from(wire: SessionKeyWire) -> Result<Self> {
        Self::new(wire.scope, wire.session)
    }
}
impl SessionKey {
    pub fn new(scope: impl Into<String>, session: impl Into<String>) -> Result<Self> {
        let key = Self {
            scope: scope.into(),
            session: session.into(),
        };
        for value in [&key.scope, &key.session] {
            if value.is_empty()
                || value.len() > 200
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b))
            {
                return Err(Error::Invalid(
                    "scope and Session require safe opaque IDs".into(),
                ));
            }
        }
        Ok(key)
    }
    pub fn scope(&self) -> &str {
        &self.scope
    }
    pub fn session_id(&self) -> &str {
        &self.session
    }
    pub(crate) fn root(&self) -> String {
        format!("v1/scopes/{}/sessions/{}", self.scope, self.session)
    }
}

#[derive(Clone, Debug)]
pub struct Limits {
    pub max_head_bytes: usize,
    pub max_active_runs: usize,
    pub max_threads: usize,
    pub max_record_bytes: usize,
    pub max_cas_attempts: usize,
    pub recent_commits: usize,
    pub cache_bytes: usize,
    pub read_ahead_bytes: u64,
    pub page_snapshot_ttl_ms: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_head_bytes: 1_048_576,
            max_active_runs: 128,
            max_threads: 128,
            max_record_bytes: 64 * 1_048_576,
            max_cas_attempts: 16,
            recent_commits: 64,
            cache_bytes: 32 * 1_048_576,
            read_ahead_bytes: 4 * 1_048_576,
            page_snapshot_ttl_ms: 15 * 60 * 1000,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObjectRef {
    pub key: String,
    pub sha256: String,
    pub offset: u64,
    pub length: u64,
    pub generation: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobRef {
    pub object: ObjectRef,
    pub media_type: String,
    pub size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunPhase {
    Pending,
    Running,
    Awaiting,
    Done,
    Cancelled,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Claim {
    pub run_id: String,
    pub owner: String,
    pub epoch: awaken_lease_contract::fence::Epoch,
    pub expires_at_ms: u64,
}
impl Claim {
    pub fn run_claim(&self) -> awaken_worker_contract::RunClaim {
        awaken_worker_contract::RunClaim {
            run_id: awaken_agent_contract::agent::run::Id(self.run_id.clone()),
            owner: self.owner.clone(),
            epoch: self.epoch.get(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct RunSlot {
    pub thread: String,
    pub activation: ObjectRef,
    pub identity: String,
    pub phase: RunPhase,
    pub claim: Option<Claim>,
    pub last_epoch: Option<awaken_lease_contract::fence::Epoch>,
    pub cancel_requested: bool,
    pub next_ordinal: u64,
    pub physical_attempt: Option<awaken_worker_contract::RunClaim>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ThreadHead {
    pub version: u64,
    pub message_end: u64,
    pub archived: bool,
    pub base: Option<ObjectRef>,
    pub effective: Option<ObjectRef>,
    pub effective_from: u64,
}
impl ThreadHead {
    pub(crate) fn empty() -> Self {
        Self {
            version: 0,
            message_end: 0,
            archived: false,
            base: None,
            effective: None,
            effective_from: 0,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CommitDescriptor {
    pub thread: String,
    pub receipt: CommitReceipt,
    pub at_ms: u64,
    pub message_start: u64,
    pub message_end: u64,
    pub previous_hash: String,
    pub commit_hash: String,
    pub body: ObjectRef,
    #[serde(default)]
    pub artifacts: Vec<BlobRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct IndexRef {
    pub object: ObjectRef,
    pub first: u64,
    pub last: u64,
    pub threads: BTreeMap<String, (u64, u64)>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct CheckpointRef {
    pub object: ObjectRef,
    pub claim: awaken_worker_contract::RunClaim,
    pub stream: Option<StreamCursor>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Head {
    pub format: u32,
    pub key: SessionKey,
    pub generation: u64,
    pub gc_generation: u64,
    pub commit_sequence: u64,
    pub chain_hash: String,
    pub archived: bool,
    pub deleted: bool,
    pub threads: BTreeMap<String, ThreadHead>,
    pub runs: BTreeMap<String, RunSlot>,
    pub order: Vec<String>,
    pub recent: Vec<CommitDescriptor>,
    pub indexes: Vec<IndexRef>,
    pub checkpoints: BTreeMap<String, CheckpointRef>,
    pub frames: BTreeMap<String, StreamLog>,
    #[serde(default)]
    pub stream_sequence: u64,
    #[serde(default)]
    pub stream_archive: Option<ObjectRef>,
    pub blobs: BTreeMap<String, BlobRef>,
    pub snapshots: BTreeMap<String, ObjectRef>,
    #[serde(default)]
    pub snapshot_deadlines: BTreeMap<String, u64>,
    #[serde(default)]
    pub components: BTreeMap<String, ObjectRef>,
    #[serde(default)]
    pub component_markers:
        BTreeMap<String, BTreeMap<String, awaken_object_storage::PublicationMarker>>,
    #[serde(default)]
    pub records: BTreeMap<String, ObjectRef>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub claim_candidates: BTreeMap<String, ClaimCandidateRef>,
}
impl Head {
    pub(crate) fn new(key: SessionKey) -> Self {
        let mut threads = BTreeMap::new();
        threads.insert(key.session.clone(), ThreadHead::empty());
        Self {
            format: 1,
            key,
            generation: 0,
            gc_generation: 0,
            commit_sequence: 0,
            chain_hash: String::new(),
            archived: false,
            deleted: false,
            threads,
            runs: BTreeMap::new(),
            order: Vec::new(),
            recent: Vec::new(),
            indexes: Vec::new(),
            checkpoints: BTreeMap::new(),
            frames: BTreeMap::new(),
            stream_sequence: 0,
            stream_archive: None,
            blobs: BTreeMap::new(),
            snapshots: BTreeMap::new(),
            snapshot_deadlines: BTreeMap::new(),
            components: BTreeMap::new(),
            component_markers: BTreeMap::new(),
            records: BTreeMap::new(),
            claim_candidates: BTreeMap::new(),
        }
    }
}
#[derive(Clone, Debug)]
pub struct HeadView {
    pub key: SessionKey,
    pub generation: u64,
    pub commit_sequence: u64,
    pub chain_hash: String,
    pub archived: bool,
    pub threads: BTreeMap<String, ThreadView>,
    pub runs: BTreeMap<String, RunPhase>,
}
#[derive(Clone, Debug)]
pub struct ThreadView {
    pub version: u64,
    pub message_end: u64,
    pub archived: bool,
}
impl From<&Head> for HeadView {
    fn from(head: &Head) -> Self {
        Self {
            key: head.key.clone(),
            generation: head.generation,
            commit_sequence: head.commit_sequence,
            chain_hash: head.chain_hash.clone(),
            archived: head.archived,
            threads: head
                .threads
                .iter()
                .map(|(id, t)| {
                    (
                        id.clone(),
                        ThreadView {
                            version: t.version,
                            message_end: t.message_end,
                            archived: t.archived,
                        },
                    )
                })
                .collect(),
            runs: head
                .runs
                .iter()
                .map(|(id, r)| (id.clone(), r.phase))
                .collect(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "body", rename_all = "snake_case")]
pub(crate) enum Record {
    RunAccepted(Box<RunDispatch>),
    StepCommitted(Box<CommitOperation>),
    Checkpoint(StreamCheckpoint),
    Transcript(Vec<Message>),
    ComponentEvent {
        component: String,
        id: String,
        kind: String,
        value: serde_json::Value,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamCursor {
    pub run: String,
    pub step: u64,
    pub response: u64,
    /// The producer's logical Step within an explicit progress stream.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assistant_step: Option<u64>,
    pub attempt: u64,
    pub next_frame: u64,
}
#[derive(Clone, Debug)]
pub struct StreamBatch {
    pub cursor: StreamCursor,
    pub sequence: u64,
    pub content_type: String,
    pub bytes: bytes::Bytes,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReadCursor {
    pub key: SessionKey,
    pub after: u64,
}
pub struct Subscription {
    pub(crate) key: SessionKey,
    pub(crate) after: u64,
    pub(crate) receiver: tokio::sync::broadcast::Receiver<u64>,
}
pub(crate) fn next(value: u64) -> Result<u64> {
    value
        .checked_add(1)
        .ok_or_else(|| Error::Capacity("sequence exhausted".into()))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ViewPart {
    pub body: ObjectRef,
    pub start: u64,
    pub end: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct FrozenView {
    pub reference: TranscriptSnapshotRef,
    pub parts: Vec<ViewPart>,
}
pub(crate) fn snapshot_key(reference: &TranscriptSnapshotRef) -> String {
    format!(
        "{}:{:?}:{}:{}",
        reference.thread_id.0, reference.view, reference.version, reference.end_seq
    )
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamReadCursor {
    pub key: SessionKey,
    pub run: String,
    pub after: u64,
}
pub struct StreamReadPage {
    pub batches: Vec<StreamBatch>,
    pub next_cursor: StreamReadCursor,
    pub watermark: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(from = "StreamFrameWire")]
pub(crate) struct StreamFrame {
    pub cursor: StreamCursor,
    pub object: ObjectRef,
    pub sequence: u64,
    pub content_type: String,
}
fn opaque_content_type() -> String {
    "application/octet-stream".into()
}
#[derive(Deserialize)]
#[serde(untagged)]
enum StreamFrameWire {
    Current {
        cursor: StreamCursor,
        object: ObjectRef,
        #[serde(default)]
        sequence: u64,
        #[serde(default = "opaque_content_type")]
        content_type: String,
    },
    Legacy((StreamCursor, ObjectRef)),
}
impl From<StreamFrameWire> for StreamFrame {
    fn from(wire: StreamFrameWire) -> Self {
        match wire {
            StreamFrameWire::Current {
                cursor,
                object,
                sequence,
                content_type,
            } => Self {
                cursor,
                object,
                sequence,
                content_type,
            },
            StreamFrameWire::Legacy((cursor, object)) => Self {
                cursor,
                object,
                sequence: 0,
                content_type: opaque_content_type(),
            },
        }
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(try_from = "StreamLogWire")]
pub(crate) struct StreamLog {
    #[serde(default)]
    pub identity: Option<StreamCursor>,
    pub next_frame: u64,
    pub tail: Vec<StreamFrame>,
    pub frontier: Vec<StreamIndexRef>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct StreamIndexRef {
    pub object: ObjectRef,
    pub first: u64,
    pub end: u64,
    pub level: u8,
    #[serde(default)]
    pub first_sequence: u64,
    #[serde(default)]
    pub last_sequence: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", content = "entries", rename_all = "snake_case")]
pub(crate) enum StreamIndex {
    Frames(Vec<StreamFrame>),
    Children(Vec<StreamIndexRef>),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum StreamLogWire {
    Legacy(Vec<(StreamCursor, ObjectRef)>),
    Indexed {
        #[serde(default)]
        identity: Option<StreamCursor>,
        next_frame: u64,
        tail: Vec<StreamFrame>,
        frontier: Vec<StreamIndexRef>,
    },
}
impl TryFrom<StreamLogWire> for StreamLog {
    type Error = Error;
    fn try_from(wire: StreamLogWire) -> Result<Self> {
        Ok(match wire {
            StreamLogWire::Indexed {
                identity,
                next_frame,
                tail,
                frontier,
            } => Self {
                identity,
                next_frame,
                tail,
                frontier,
            },
            StreamLogWire::Legacy(tail) => {
                if tail
                    .iter()
                    .enumerate()
                    .any(|(i, (cursor, _))| cursor.next_frame != i as u64 + 1)
                {
                    return Err(Error::Corrupt("legacy stream contains gaps".into()));
                }
                let identity = tail.first().map(|(cursor, _)| {
                    let mut identity = cursor.clone();
                    identity.next_frame = 0;
                    identity
                });
                Self {
                    next_frame: tail.len() as u64,
                    tail: tail
                        .into_iter()
                        .map(|(cursor, object)| StreamFrame {
                            cursor,
                            object,
                            sequence: 0,
                            content_type: opaque_content_type(),
                        })
                        .collect(),
                    frontier: Vec::new(),
                    identity,
                }
            }
        })
    }
}

/// Subscription resumes from the last durably read Session ingest sequence.
pub struct StreamSubscription {
    pub(crate) cursor: StreamReadCursor,
    pub(crate) receiver: tokio::sync::broadcast::Receiver<u64>,
}
impl StreamSubscription {
    pub fn cursor(&self) -> StreamReadCursor {
        self.cursor.clone()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub(crate) struct ClaimCandidateRef {
    pub(crate) path: String,
    pub(crate) due_at_ms: u64,
}
