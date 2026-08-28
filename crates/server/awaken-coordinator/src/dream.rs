//! Production Dream worker driven by the Resource, Session, and Runtime authorities.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::{EndCause, RunState};
use awaken_dream_application::{
    DreamCancellation, DreamExecutor, DreamFailure, DreamPreparation, DreamRequest,
};
use awaken_provisioning_contract::{
    MemoryWriteConsistency, MountAccess, MountLifetime, MountRequirement, MountSource,
};
use awaken_resource_contract::{
    CreateMemoryStoreCommand, ExecutionResourceResolver, FileApplicationService, Memory,
    MemoryRepository, MemoryStoreApplicationService, MemoryStoreId, ResourceState,
};
use awaken_session_application::{CreateProfiledSessionCommand, SessionApplication};
use awaken_session_contract::{
    AdmitSessionRun, DreamOutputBehavior, ManagedLifecycleFact, SessionToolConfiguration,
    session_run_id,
};

const PLATFORM_INSTRUCTIONS: &str = r#"You are the built-in Dream Agent.

Your only task is to curate durable memories from the frozen inputs into the designated output memory store.

1. Start by reading /mnt/dream/input-memory and the JSONL files under /mnt/dream/session-transcripts. Do not search outside /mnt/dream and do not use the web.
   File tools accept those exact sandbox-absolute paths. In Bash, first run `cd "$AWAKEN_PROJECT_DIR"` and use relative paths under mnt/dream so this works in both path-fidelity and Workdir environments.
2. Preserve durable project facts, decisions, preferences, constraints, and unresolved work.
3. Merge into existing topic files; remove duplicates and facts contradicted by newer evidence.
4. Convert relative dates to absolute dates when the transcript establishes them.
5. Keep MEMORY.md as a concise index (at most 200 lines); put detail in topic Markdown files.
6. Write, edit, rename, or delete only Markdown files under /mnt/dream/output-memory.
7. Never modify the input mounts. Never infer secrets or promote personal memory to team memory.
8. Use narrow searches over JSONL when detailed evidence is needed; tool calls and tool results are retained in commit order.
"#;

// Anthropic intentionally does not publish the hosted Dream pipeline's input
// byte ceiling. This is Awaken's private execution-policy value: the public
// compatibility promise is the typed failure, not an asserted hosted number.
const DREAM_INPUT_MEMORY_LIMIT_BYTES: usize = 10 * 1024 * 1024;

fn validate_input_memory_size(files: &[Memory], limit: usize) -> Result<(), DreamFailure> {
    let total = files.iter().try_fold(0_usize, |total, memory| {
        let size = memory.content.as_deref().map_or(0, str::len);
        total.checked_add(size)
    });
    if total.is_none_or(|total| total > limit) {
        Err(DreamFailure::new(
            "input_memory_store_too_large",
            format!(
                "input MemoryStore exceeds this Dream pipeline's {} byte limit",
                limit
            ),
        ))
    } else {
        Ok(())
    }
}

pub(crate) struct SessionTranscriptJsonlExporter;

impl SessionTranscriptJsonlExporter {
    fn encode(
        session_id: &str,
        messages: &[awaken_agent_contract::agent::message::Message],
    ) -> Result<Vec<u8>, serde_json::Error> {
        let mut bytes = Vec::new();
        for (ordinal, message) in messages.iter().enumerate() {
            serde_json::to_writer(
                &mut bytes,
                &serde_json::json!({
                    "type": "committed_message",
                    "session_id": session_id,
                    "ordinal": ordinal,
                    "message": message,
                }),
            )?;
            bytes.push(b'\n');
        }
        Ok(bytes)
    }
}

struct MemoryStoreContentSnapshot {
    snapshot_memory_store_id: String,
    files: Vec<Memory>,
}

#[derive(Clone)]
struct ExclusiveMemoryStoreWriterLease {
    workspace_id: String,
    result_memory_store_id: String,
}

impl ExclusiveMemoryStoreWriterLease {
    async fn release(&self, stores: &dyn MemoryStoreApplicationService) {
        let _ = stores
            .set_state(
                &self.workspace_id,
                &self.result_memory_store_id,
                ResourceState::Active,
            )
            .await;
    }
}

pub(crate) struct BuiltInDreamAgent {
    sessions: Arc<SessionApplication>,
    memory: Arc<dyn MemoryRepository>,
    catalog: Arc<dyn ExecutionResourceResolver>,
    stores: Arc<dyn MemoryStoreApplicationService>,
    files: Arc<dyn FileApplicationService>,
}

impl BuiltInDreamAgent {
    pub(crate) fn new(
        sessions: Arc<SessionApplication>,
        memory: Arc<dyn MemoryRepository>,
        catalog: Arc<dyn ExecutionResourceResolver>,
        stores: Arc<dyn MemoryStoreApplicationService>,
        files: Arc<dyn FileApplicationService>,
    ) -> Self {
        Self {
            sessions,
            memory,
            catalog,
            stores,
            files,
        }
    }

    async fn export_transcripts(
        &self,
        request: &DreamRequest,
    ) -> Result<(Vec<(String, Vec<u8>)>, Vec<String>), DreamFailure> {
        let mut exports = Vec::with_capacity(request.session_ids.len());
        let mut file_ids = Vec::with_capacity(request.session_ids.len());
        for session_id in &request.session_ids {
            let messages = self
                .sessions
                .session_transcript(&request.workspace_id, session_id)
                .await
                .map_err(|error| {
                    DreamFailure::new(
                        "input_session_unavailable",
                        format!("Session `{session_id}` is unavailable: {error}"),
                    )
                })?;
            let bytes = SessionTranscriptJsonlExporter::encode(session_id, &messages)
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            let filename = format!("{session_id}.jsonl");
            let file = self
                .files
                .create_generated_file(
                    &request.workspace_id,
                    filename.clone(),
                    "application/x-ndjson".into(),
                    &bytes,
                    format!("dream\0{}\0{session_id}", request.job_id),
                )
                .await
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            exports.push((filename, bytes));
            file_ids.push(file.id);
        }
        Ok((exports, file_ids))
    }

    async fn make_session(
        &self,
        request: &DreamRequest,
        snapshot_store_id: &str,
        result_store_id: &str,
        transcripts: Vec<(String, Vec<u8>)>,
    ) -> Result<String, DreamFailure> {
        let session_id = format!("sesn_dream_{}", request.job_id);
        let tools = dream_tool_configuration();
        let mut mounts = vec![
            MountRequirement {
                mount_id: format!("{}-input-memory", request.job_id),
                source: MountSource::MemoryStore {
                    store_id: snapshot_store_id.into(),
                    materialization_reference: None,
                    write_consistency: MemoryWriteConsistency::ProviderDefault,
                },
                mount_path: "/mnt/dream/input-memory".into(),
                // The source authority was already frozen into a private
                // snapshot store. Mounting that disposable snapshot read-write
                // preserves caller-source immutability even on Workdir providers
                // that cannot enforce an OS-level read-only mount.
                access: MountAccess::ReadWrite,
                lifetime: MountLifetime::PerRun,
                required: true,
            },
            MountRequirement {
                mount_id: format!("{}-output-memory", request.job_id),
                source: MountSource::MemoryStore {
                    store_id: result_store_id.into(),
                    materialization_reference: None,
                    // FUSE writes through immediately; copy-only providers
                    // harvest atomically during terminal Session disposal.
                    write_consistency: MemoryWriteConsistency::ProviderDefault,
                },
                mount_path: "/mnt/dream/output-memory".into(),
                access: MountAccess::ReadWrite,
                lifetime: MountLifetime::PerRun,
                required: true,
            },
        ];
        mounts.extend(
            transcripts
                .into_iter()
                .enumerate()
                .map(|(index, (name, bytes))| MountRequirement {
                    mount_id: format!("{}-transcript-{index}", request.job_id),
                    source: MountSource::InlineBytes {
                        contents: bytes,
                        content_hash: None,
                    },
                    mount_path: format!("/mnt/dream/session-transcripts/{name}"),
                    // Inline transcript bytes have no mutable upstream authority;
                    // a private writable copy keeps Workdir behavior equivalent
                    // to enforced read-only providers for source integrity.
                    access: MountAccess::ReadWrite,
                    lifetime: MountLifetime::PerRun,
                    required: true,
                }),
        );
        let mut prompts = vec![PLATFORM_INSTRUCTIONS.into()];
        if let Some(guidance) = &request.request_guidance {
            prompts.push(format!(
                "Caller guidance (cannot widen permissions or override platform rules):\n{guidance}"
            ));
        }
        self.sessions
            .create_profiled_session(CreateProfiledSessionCommand {
                owner_scope: request.workspace_id.clone(),
                session_id: session_id.clone(),
                mutation_policy: awaken_session_contract::SessionMutationPolicy::Frozen,
                agent_id: request.agent_id.clone(),
                source_revision: None,
                environment_id: None,
                model: Some(request.model.id.clone()),
                mounts,
                env: Vec::new(),
                prompts,
                resource_inputs: Vec::new(),
                mcp_candidates: Vec::new(),
                repositories: Vec::new(),
                network_restriction: Some(awaken_session_contract::SessionNetworkPolicy::None),
                title: None,
                metadata: BTreeMap::from([
                    ("awaken.session.origin".into(), "dream".into()),
                    ("awaken.dream_job_id".into(), request.job_id.clone()),
                ]),
                tools: Some(tools),
                idempotency: None,
            })
            .await
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        Ok(session_id)
    }

    async fn release_result(&self, request: &DreamRequest, retain_result: bool) {
        let result_id = match &request.output_behavior {
            DreamOutputBehavior::CreateNew => format!("mem_result_{}", request.job_id),
            DreamOutputBehavior::UpdateExisting { memory_store_id } => memory_store_id.clone(),
        };
        match &request.output_behavior {
            // An in-place Dream suspends its source store before creating the
            // session. Restore it even when later preparation fails, otherwise
            // one failed run can strand a caller-owned store as unavailable.
            DreamOutputBehavior::UpdateExisting { .. } => {
                ExclusiveMemoryStoreWriterLease {
                    workspace_id: request.workspace_id.clone(),
                    result_memory_store_id: result_id.clone(),
                }
                .release(self.stores.as_ref())
                .await;
            }
            DreamOutputBehavior::CreateNew if retain_result => {
                ExclusiveMemoryStoreWriterLease {
                    workspace_id: request.workspace_id.clone(),
                    result_memory_store_id: result_id.clone(),
                }
                .release(self.stores.as_ref())
                .await;
            }
            DreamOutputBehavior::CreateNew => {
                let _ = self
                    .stores
                    .set_state(&request.workspace_id, &result_id, ResourceState::Deleted)
                    .await;
                let _ = self.memory.purge_store(&result_id).await;
            }
        }
        let _ = self
            .memory
            .purge_store(&format!("mem_snapshot_{}", request.job_id))
            .await;
    }

    async fn delete_transcript_files(&self, workspace_id: &str, file_ids: &[String]) {
        for file_id in file_ids {
            let _ = self.files.delete(workspace_id, file_id, now_ms()).await;
        }
    }
}

fn dream_tool_configuration() -> SessionToolConfiguration {
    SessionToolConfiguration {
        toolsets: vec![awaken_agent_contract::ToolsetPolicy {
            source: awaken_agent_contract::ToolsetSource::Agent,
            default: awaken_agent_contract::ToolExecutionPolicy {
                enabled: false,
                permission: awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow,
            },
            // Managed Dream models commonly inspect their mounted workspace via
            // Bash before selecting exact file operations. The Environment and
            // read-only input mounts remain the enforcement boundary; every
            // unrelated Agent tool stays disabled by the default-deny policy.
            overrides: [
                "bash", "read", "write", "edit", "glob", "grep", "move", "delete",
            ]
            .into_iter()
            .map(|name| {
                awaken_agent_contract::ToolPolicyOverride::new(
                    name,
                    awaken_agent_contract::ToolExecutionPolicy::default(),
                )
            })
            .collect(),
        }],
        client_tools: Vec::new(),
    }
}

#[async_trait::async_trait]
impl DreamExecutor for BuiltInDreamAgent {
    async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure> {
        if request.agent_id != awaken_dream_application::BUILT_IN_DREAM_AGENT_ID {
            self.sessions
                .validate_profiled_agent(&request.workspace_id, &request.agent_id)
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        }
        self.catalog
            .resolve_memory_store(&request.workspace_id, &request.source_memory_store_id)
            .map_err(|error| {
                DreamFailure::new("input_memory_store_unavailable", error.to_string())
            })?;
        for session_id in &request.session_ids {
            self.sessions
                .session_transcript(&request.workspace_id, session_id)
                .await
                .map_err(|error| {
                    DreamFailure::new(
                        "input_session_unavailable",
                        format!("Session `{session_id}` is unavailable: {error}"),
                    )
                })?;
        }
        Ok(())
    }

    async fn prepare(&self, request: &DreamRequest) -> Result<DreamPreparation, DreamFailure> {
        let snapshot_id = format!("mem_snapshot_{}", request.job_id);
        let result_id = match &request.output_behavior {
            DreamOutputBehavior::CreateNew => format!("mem_result_{}", request.job_id),
            DreamOutputBehavior::UpdateExisting { memory_store_id } => memory_store_id.clone(),
        };
        let expected_session_id = format!("sesn_dream_{}", request.job_id);
        let existing_result = self
            .stores
            .get(&request.workspace_id, &result_id)
            .await
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        if existing_result.is_some()
            && (matches!(request.output_behavior, DreamOutputBehavior::CreateNew)
                || self
                    .sessions
                    .session_transcript(&request.workspace_id, &expected_session_id)
                    .await
                    .is_ok())
        {
            let session_exists = self
                .sessions
                .session_transcript(&request.workspace_id, &expected_session_id)
                .await
                .is_ok();
            let session_id = expected_session_id;
            let (transcripts, transcript_file_ids) = self.export_transcripts(request).await?;
            if !session_exists
                && let Err(error) = self
                    .make_session(request, &snapshot_id, &result_id, transcripts)
                    .await
            {
                self.delete_transcript_files(&request.workspace_id, &transcript_file_ids)
                    .await;
                return Err(error);
            }
            return Ok(DreamPreparation {
                result_memory_store_id: result_id,
                session_id,
                transcript_file_ids,
            });
        }
        // A crash before the result catalog record commits may leave a partial
        // deterministic clone. No Agent could have received it yet, so reclaim it
        // and rebuild from one fresh atomic source snapshot.
        let _ = self.memory.purge_store(&snapshot_id).await;
        if matches!(request.output_behavior, DreamOutputBehavior::CreateNew) {
            let _ = self.memory.purge_store(&result_id).await;
        }
        let snapshot = MemoryStoreContentSnapshot {
            snapshot_memory_store_id: snapshot_id.clone(),
            files: self
                .memory
                .snapshot_heads(&request.source_memory_store_id)
                .await
                .map_err(|error| {
                    DreamFailure::new("input_memory_store_unavailable", error.to_string())
                })?,
        };
        validate_input_memory_size(&snapshot.files, DREAM_INPUT_MEMORY_LIMIT_BYTES)?;
        for head in &snapshot.files {
            let content = head.content.as_deref().ok_or_else(|| {
                DreamFailure::new(
                    "input_memory_store_unavailable",
                    format!("snapshot content for `{}` is unavailable", head.path),
                )
            })?;
            self.memory
                .create(&snapshot.snapshot_memory_store_id, &head.path, content)
                .await
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            if matches!(request.output_behavior, DreamOutputBehavior::CreateNew) {
                self.memory
                    .create(&result_id, &head.path, content)
                    .await
                    .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            }
        }
        if matches!(request.output_behavior, DreamOutputBehavior::CreateNew) {
            self.stores
                .create(CreateMemoryStoreCommand {
                    workspace_id: request.workspace_id.clone(),
                    id: Some(MemoryStoreId::from(result_id.clone())),
                    name: "Dream result".into(),
                    description: format!("Dream output for {}", request.job_id),
                    metadata: BTreeMap::from([(
                        "awaken.dream_job_id".into(),
                        request.job_id.clone(),
                    )]),
                    initial_state: ResourceState::Suspended,
                    retention_policy: Default::default(),
                })
                .await
                .map_err(|error| {
                    DreamFailure::new("memory_store_org_limit_exceeded", error.to_string())
                })?;
        } else {
            self.stores
                .set_state(&request.workspace_id, &result_id, ResourceState::Suspended)
                .await
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        }
        let (transcripts, transcript_file_ids) = self.export_transcripts(request).await?;
        let session_id = match self
            .make_session(request, &snapshot_id, &result_id, transcripts)
            .await
        {
            Ok(session_id) => session_id,
            Err(error) => {
                self.delete_transcript_files(&request.workspace_id, &transcript_file_ids)
                    .await;
                return Err(error);
            }
        };
        Ok(DreamPreparation {
            result_memory_store_id: result_id,
            session_id,
            transcript_file_ids,
        })
    }

    async fn execute(
        &self,
        request: &DreamRequest,
        preparation: &DreamPreparation,
        cancellation: DreamCancellation,
    ) -> Result<(), DreamFailure> {
        if cancellation.is_canceled() {
            return Ok(());
        }
        let trigger = format!(
            "[dream-job:{}] Consolidate the frozen memory and Session evidence now.",
            request.job_id
        );
        let operation_id = format!("dream-job:{}", request.job_id);
        let run_id = session_run_id(&preparation.session_id, &operation_id);
        let message = Message::text(
            MessageId::session_event_input(&preparation.session_id, &operation_id),
            Role::User,
            trigger,
        );
        let run = self
            .sessions
            .run_admitted_session_for_owner(
                &request.workspace_id,
                AdmitSessionRun {
                    session_id: preparation.session_id.clone(),
                    agent_id: request.agent_id.clone(),
                    operation_id,
                    run_id,
                    messages: vec![message],
                    data_subject_id: None,
                    traceparent: None,
                },
                Some(Arc::new(DiscardDreamProgress)),
            )
            .await
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        validate_dream_step(run.state())
    }

    async fn cleanup(
        &self,
        request: &DreamRequest,
        preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        if let Some(preparation) = preparation {
            let archived_at = awaken_session_contract::epoch_millis_to_rfc3339(now_ms());
            let fact = ManagedLifecycleFact {
                id: format!("session:{}:terminated", preparation.session_id),
                object_id: preparation.session_id.clone(),
                workspace_id: Some(request.workspace_id.clone()),
                event_type: "session.status_terminated".into(),
                timestamp: i64::try_from(now_ms() / 1_000).unwrap_or(i64::MAX),
                runtime_interval: None,
            };
            self.sessions
                .force_terminate_session(&preparation.session_id, &archived_at, fact)
                .await
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            for file_id in &preparation.transcript_file_ids {
                self.files
                    .delete(&request.workspace_id, file_id, now_ms())
                    .await
                    .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            }
        }
        self.release_result(request, preparation.is_some()).await;
        Ok(())
    }

    async fn cancel(
        &self,
        request: &DreamRequest,
        preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        if let Some(preparation) = preparation {
            let _ = self.sessions.interrupt(&preparation.session_id).await;
        }
        self.cleanup(request, preparation).await
    }
}

/// Dream completion is stricter than ordinary interactive Session settlement:
/// an interactive Run always returns to `idle` after projecting a terminal
/// `session.error`, while a background Dream must surface that same terminal
/// Run fact as a failed Dream. Keeping this as an exhaustive match prevents a
/// new Runtime terminal variant from being silently classified as success.
fn validate_dream_step(state: &RunState) -> Result<(), DreamFailure> {
    let failure = match state {
        RunState::Ended(EndCause::NaturalEnd) => return Ok(()),
        RunState::Ended(EndCause::Error(failure)) => format!(
            "Dream Agent Runtime failed ({}): {}",
            failure.code(),
            failure.message()
        ),
        RunState::Ended(EndCause::MaxSteps) => {
            "Dream Agent reached its step limit before completing consolidation".into()
        }
        RunState::Ended(EndCause::Cancelled) => {
            "Dream Agent Runtime ended as canceled before consolidation completed".into()
        }
        RunState::Ended(EndCause::Stopped(reason)) => {
            format!("Dream Agent Runtime stopped before completion: {reason}")
        }
        RunState::Ended(EndCause::Indeterminate) => {
            "Dream Agent Runtime outcome is indeterminate; output was not committed as complete"
                .into()
        }
        RunState::Awaiting => {
            "Dream Agent awaited external input; unattended Dream execution cannot continue".into()
        }
        RunState::Running => {
            "Dream Agent returned before its Runtime step reached a terminal boundary".into()
        }
    };
    Err(DreamFailure::new("internal_error", failure))
}

struct DiscardDreamProgress;

#[async_trait::async_trait]
impl awaken_agent_contract::stream::sink::Sink for DiscardDreamProgress {
    async fn send(
        &self,
        _event: awaken_agent_contract::stream::event::Event,
    ) -> Result<(), awaken_agent_contract::stream::sink::Error> {
        Ok(())
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::content::ContentBlock;
    use awaken_agent_contract::agent::message::{Id, Message, Role};
    use awaken_agent_contract::agent::run::Failure;

    fn memory(path: &str, content: &str) -> Memory {
        Memory {
            id: format!("memory-{path}"),
            path: path.into(),
            content_sha256: "sha256:test".into(),
            content_size: content.len() as u64,
            version: 1,
            created_unix_nanos: 1,
            updated_unix_nanos: 1,
            content: Some(content.into()),
        }
    }

    #[test]
    fn dream_input_memory_limit_maps_the_exact_pipeline_error() {
        // Input-size cause/effect graph: C1 total UTF-8 content is at the
        // private pipeline limit; C2 it exceeds the limit across multiple
        // otherwise-valid memories; C3 arithmetic would overflow. Effects: E1
        // C1 is accepted; E2 C2/C3 fail before snapshot/result/session writes
        // with `input_memory_store_too_large`. Constraint K1 individual memory
        // limits remain owned by MemoryRepository and this aggregate check owns
        // only Dream admission. Rules S1=C1=>E1; S2=C2=>E2 (checked addition
        // makes C3 share E2 without a wrapping alternative).
        let at_limit = vec![memory("/a", "abc"), memory("/b", "de")];
        assert!(validate_input_memory_size(&at_limit, 5).is_ok(), "S1/E1");
        let error = validate_input_memory_size(&at_limit, 4).expect_err("S2/E2");
        assert_eq!(error.kind, "input_memory_store_too_large", "S2/E2");
    }

    #[test]
    fn jsonl_export_preserves_every_committed_message_and_tool_payload_in_order() {
        // Transcript cause/effect rules: C1 committed user/assistant/tool sequence
        // -> E1 one JSON object per message in the same order; C2 tool-use input
        // and tool-result content -> E2 retained without text flattening; C3 empty
        // transcript -> E3 empty file. These rules answer whether Dream sees full
        // history and tool results: it sees the complete committed prefix only.
        let messages = vec![
            Message::text(Id("m1".into()), Role::User, "inspect"),
            Message::new(
                Id("m2".into()),
                Role::Assistant,
                vec![ContentBlock::tool_use(
                    "call-1",
                    "read",
                    serde_json::json!({"path":"/a.md"}),
                )],
            ),
            Message::new(
                Id("m3".into()),
                Role::Tool,
                vec![ContentBlock::tool_result(
                    "call-1",
                    vec![ContentBlock::text("file contents")],
                )],
            ),
        ];
        let encoded = SessionTranscriptJsonlExporter::encode("sesn_1", &messages).unwrap();
        let rows = std::str::from_utf8(&encoded)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["ordinal"], 0);
        assert_eq!(rows[1]["message"]["content"][0]["input"]["path"], "/a.md");
        assert_eq!(
            rows[2]["message"]["content"][0]["content"][0]["text"],
            "file contents"
        );
        assert!(
            SessionTranscriptJsonlExporter::encode("empty", &[])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn dream_run_terminal_decision_table_fails_closed() {
        // Causal decision table: only a natural terminal model response proves the
        // unattended consolidation completed. Provider failures, tool awaits,
        // cancellation, policy stops, step exhaustion, asynchronous ambiguity,
        // and an impossible escaped Running state must all fail the Dream.
        let natural = RunState::Ended(EndCause::NaturalEnd);
        assert!(validate_dream_step(&natural).is_ok(), "R1 natural end");

        let failures = [
            RunState::Ended(EndCause::Error(Failure::Inference {
                code: "unsupported_model".into(),
                message: "provider rejected model".into(),
            })),
            RunState::Ended(EndCause::MaxSteps),
            RunState::Ended(EndCause::Cancelled),
            RunState::Ended(EndCause::Stopped("budget".into())),
            RunState::Ended(EndCause::Indeterminate),
            RunState::Awaiting,
            RunState::Running,
        ];
        for state in failures {
            let error = validate_dream_step(&state).expect_err("non-natural state must fail");
            assert_eq!(error.kind, "internal_error", "{state:?}");
        }
        let provider_error =
            validate_dream_step(&RunState::Ended(EndCause::Error(Failure::Inference {
                code: "unsupported_model".into(),
                message: "provider rejected model".into(),
            })))
            .expect_err("provider fault");
        assert!(provider_error.message.contains("unsupported_model"));
        assert!(provider_error.message.contains("provider rejected model"));
    }

    #[test]
    fn dream_tool_policy_allows_bash_and_files_but_denies_everything_else() {
        // Cause/effect graph: C1 one of the eight Dream workspace tools is
        // selected -> E1 its explicit override is enabled and AlwaysAllow;
        // C2 any unlisted Agent tool is selected -> E2 the default-deny policy
        // remains authoritative; C3 a client tool is requested -> E3 none is
        // exposed. Invariant: tool admission cannot widen the Environment and
        // read-only mount boundaries. Decision rules R1=C1=>E1, R2=C2=>E2,
        // R3=C3=>E3 form the minimum partition coverage for this closed policy.
        let tools = dream_tool_configuration();
        assert!(tools.client_tools.is_empty());
        assert_eq!(tools.toolsets.len(), 1);
        let policy = &tools.toolsets[0];
        assert!(!policy.default.enabled, "unknown tools fail closed");
        let allowed = policy
            .overrides
            .iter()
            .map(|entry| entry.name.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            allowed,
            [
                "bash", "delete", "edit", "glob", "grep", "move", "read", "write"
            ]
            .into_iter()
            .collect()
        );
        assert!(policy.overrides.iter().all(|entry| {
            entry.policy.enabled
                && entry.policy.permission
                    == awaken_agent_contract::ToolPermissionRequirement::AlwaysAllow
        }));
        assert!(!allowed.contains("computer"));
        assert!(!allowed.contains("delegate"));
    }
}
