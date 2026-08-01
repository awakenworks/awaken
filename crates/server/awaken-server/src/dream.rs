//! Production dream worker composed from existing authorities.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_memory_store::{Memory, MemoryRepository};
use awaken_protocol_managed::types::{DreamUsage, InboundEvent, SendEventsRequest};
use awaken_protocol_managed::{
    DreamCancellation, DreamFailure, DreamPreparation, DreamRequest, DreamWorker, ManagedState,
};
use awaken_provisioning_contract::{
    MemoryWriteConsistency, MountAccess, MountLifetime, MountRequirement, MountSource,
};
use awaken_resource_contract::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, MemoryStoreId, ResourceCatalog,
    ResourceState, ResourceTimestamps,
};
use awaken_session_contract::{
    ApplicationSessionContribution, ApplicationSessionContributionApi, ApplicationSessionInput,
};

use crate::SharedHost;

const PLATFORM_INSTRUCTIONS: &str = r#"You are the built-in Dream Agent.

Your only task is to curate durable memories from the frozen inputs into the independent output memory store.

1. Read /mnt/dream/input-memory and the JSONL files under /mnt/dream/session-transcripts.
2. Preserve durable project facts, decisions, preferences, constraints, and unresolved work.
3. Merge into existing topic files; remove duplicates and facts contradicted by newer evidence.
4. Convert relative dates to absolute dates when the transcript establishes them.
5. Keep MEMORY.md as a concise index (at most 200 lines); put detail in topic Markdown files.
6. Write, edit, rename, or delete only Markdown files under /mnt/dream/output-memory.
7. Never modify the input mounts. Never infer secrets or promote personal memory to team memory.
8. Use narrow searches over JSONL when detailed evidence is needed; tool calls and tool results are retained in commit order.
"#;

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
    fn release(&self, catalog: &dyn ResourceCatalog) {
        let _ = catalog.set_memory_state(
            &self.workspace_id,
            &self.result_memory_store_id,
            ResourceState::Active,
        );
    }
}

pub(crate) struct BuiltInDreamAgent {
    managed: Arc<ManagedState>,
    host: Arc<SharedHost>,
    memory: Arc<dyn MemoryRepository>,
    catalog: Arc<dyn ResourceCatalog>,
}

impl BuiltInDreamAgent {
    pub(crate) fn new(
        managed: Arc<ManagedState>,
        host: Arc<SharedHost>,
        memory: Arc<dyn MemoryRepository>,
        catalog: Arc<dyn ResourceCatalog>,
    ) -> Self {
        Self {
            managed,
            host,
            memory,
            catalog,
        }
    }

    fn memory_definition(
        request: &DreamRequest,
        id: &str,
        name: &str,
        state: ResourceState,
    ) -> MemoryStoreDefinition {
        MemoryStoreDefinition {
            id: MemoryStoreId::from(id.to_string()),
            workspace_id: request.workspace_id.clone(),
            name: name.into(),
            description: format!("Dream output for {}", request.job_id),
            metadata: BTreeMap::from([("awaken.dream_job_id".into(), request.job_id.clone())]),
            state,
            current_config_version: ConfigVersion::INITIAL,
            timestamps: ResourceTimestamps::default(),
        }
    }

    fn memory_config(id: &str) -> MemoryStoreConfigVersion {
        MemoryStoreConfigVersion {
            memory_store_id: MemoryStoreId::from(id.to_string()),
            version: ConfigVersion::INITIAL,
            recall_policy: Default::default(),
            extraction_policy: Default::default(),
            retention_policy: Default::default(),
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
                .managed
                .dream_transcript(&request.workspace_id, session_id)
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
                .host
                .file_application()
                .ok_or_else(|| {
                    DreamFailure::new("internal_error", "File application is unavailable")
                })?
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
        let create = serde_json::from_value(serde_json::json!({
            "agent": {
                "id": request.agent_selection.agent_id.clone(),
                "type": "agent_with_overrides",
                "model": request.model,
                "tools": [{
                    "type": "agent_toolset_20260401",
                    "default_config": {
                        "enabled": false,
                        "permission_policy": {"type":"always_allow"}
                    },
                    "configs": [
                        {"name":"read", "enabled":true, "permission_policy":{"type":"always_allow"}},
                        {"name":"write", "enabled":true, "permission_policy":{"type":"always_allow"}},
                        {"name":"edit", "enabled":true, "permission_policy":{"type":"always_allow"}},
                        {"name":"glob", "enabled":true, "permission_policy":{"type":"always_allow"}},
                        {"name":"grep", "enabled":true, "permission_policy":{"type":"always_allow"}},
                        {"name":"move", "enabled":true, "permission_policy":{"type":"always_allow"}},
                        {"name":"delete", "enabled":true, "permission_policy":{"type":"always_allow"}}
                    ]
                }]
            },
            "application_contribution_required": true,
            "metadata": {
                "awaken.session.origin": "dream",
                "awaken.dream_job_id": request.job_id,
            }
        }))
        .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        self.managed
            .create_application_session(
                session_id.clone(),
                create,
                Some(request.workspace_id.clone()),
            )
            .await
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;

        let mut mounts = vec![
            MountRequirement {
                mount_id: format!("{}-input-memory", request.job_id),
                source: MountSource::MemoryStore {
                    store_id: snapshot_store_id.into(),
                    materialization_reference: None,
                    write_consistency: MemoryWriteConsistency::ProviderDefault,
                },
                mount_path: "/mnt/dream/input-memory".into(),
                access: MountAccess::ReadOnly,
                lifetime: MountLifetime::PerRun,
                required: true,
            },
            MountRequirement {
                mount_id: format!("{}-output-memory", request.job_id),
                source: MountSource::MemoryStore {
                    store_id: result_store_id.into(),
                    materialization_reference: None,
                    write_consistency: MemoryWriteConsistency::WriteThroughRequired,
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
                    access: MountAccess::ReadOnly,
                    lifetime: MountLifetime::PerRun,
                    required: true,
                }),
        );
        let mount_values = mounts
            .into_iter()
            .map(serde_json::to_value)
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        let mut prompts = vec![PLATFORM_INSTRUCTIONS.into()];
        if let Some(guidance) = &request.request_guidance {
            prompts.push(format!(
                "Caller guidance (cannot widen permissions or override platform rules):\n{guidance}"
            ));
        }
        let input = ApplicationSessionInput {
            mounts: mount_values,
            prompts,
            network_restriction: Some(awaken_protocol_managed::SessionNetworkPolicy::None),
            ..Default::default()
        };
        self.managed
            .contribute_application(ApplicationSessionContribution {
                session_id: session_id.clone(),
                application_fingerprint: input.fingerprint(),
                input,
            })
            .await
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        self.managed
            .realize_application_session(&session_id)
            .await
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        Ok(session_id)
    }

    async fn release_result(&self, request: &DreamRequest, retain_result: bool) {
        let result_id = format!("mem_result_{}", request.job_id);
        if retain_result {
            ExclusiveMemoryStoreWriterLease {
                workspace_id: request.workspace_id.clone(),
                result_memory_store_id: result_id.clone(),
            }
            .release(self.catalog.as_ref());
        } else {
            let _ = self.catalog.set_memory_state(
                &request.workspace_id,
                &result_id,
                ResourceState::Deleted,
            );
            let _ = self.memory.purge_store(&result_id).await;
        }
        let _ = self
            .memory
            .purge_store(&format!("mem_snapshot_{}", request.job_id))
            .await;
    }

    async fn delete_transcript_files(&self, workspace_id: &str, file_ids: &[String]) {
        for file_id in file_ids {
            if let Some(files) = self.host.file_application() {
                let _ = files.delete(workspace_id, file_id, now_ms()).await;
            }
        }
    }
}

#[async_trait::async_trait]
impl DreamWorker for BuiltInDreamAgent {
    async fn validate_inputs(&self, request: &DreamRequest) -> Result<(), DreamFailure> {
        self.managed
            .validate_dream_agent(&request.workspace_id, &request.agent_selection.agent_id)
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        self.catalog
            .resolve_memory_store(&request.workspace_id, &request.source_memory_store_id)
            .map_err(|error| {
                DreamFailure::new("input_memory_store_unavailable", error.to_string())
            })?;
        for session_id in &request.session_ids {
            self.managed
                .dream_transcript(&request.workspace_id, session_id)
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
        let result_id = format!("mem_result_{}", request.job_id);
        let expected_session_id = format!("sesn_dream_{}", request.job_id);
        let existing_result = self
            .catalog
            .memory_store(&request.workspace_id, &result_id)
            .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        if existing_result.is_some() {
            let session_exists = self
                .managed
                .dream_transcript(&request.workspace_id, &expected_session_id)
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
        let _ = self.memory.purge_store(&result_id).await;
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
            self.memory
                .create(&result_id, &head.path, content)
                .await
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        }
        self.catalog
            .create_memory_store(
                Self::memory_definition(
                    request,
                    &result_id,
                    "Dream result",
                    ResourceState::Suspended,
                ),
                Self::memory_config(&result_id),
            )
            .map_err(|error| {
                DreamFailure::new("memory_store_org_limit_exceeded", error.to_string())
            })?;
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
    ) -> Result<DreamUsage, DreamFailure> {
        if cancellation.is_canceled() {
            return Ok(DreamUsage::default());
        }
        let trigger = format!(
            "[dream-job:{}] Consolidate the frozen memory and Session evidence now.",
            request.job_id
        );
        let already_executed = self
            .managed
            .dream_transcript(&request.workspace_id, &preparation.session_id)
            .await
            .ok()
            .is_some_and(|messages| {
                messages.iter().any(|message| {
                    serde_json::to_string(message)
                        .is_ok_and(|serialized| serialized.contains(&trigger))
                })
            });
        if already_executed {
            let session = self
                .managed
                .get_session(&preparation.session_id)
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            if session.status == "failed" {
                return Err(DreamFailure::new(
                    "internal_error",
                    "the recovered Dream Agent Session failed",
                ));
            }
            return Ok(DreamUsage {
                cache_creation_input_tokens: session.usage.cache_creation_input_tokens,
                cache_read_input_tokens: session.usage.cache_read_input_tokens,
                input_tokens: session.usage.input_tokens,
                output_tokens: session.usage.output_tokens,
            });
        }
        let content = vec![awaken_agent_contract::agent::content::ContentBlock::text(
            trigger,
        )];
        let run = self
            .managed
            .send_events(
                &preparation.session_id,
                SendEventsRequest {
                    events: vec![InboundEvent::UserMessage {
                        content,
                        session_thread_id: None,
                        model: None,
                    }],
                    user_profile_id: None,
                },
            )
            .await;
        run.map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        let session = self.managed.get_session(&preparation.session_id).ok();
        let usage = session.map(|session| session.usage).unwrap_or_default();
        Ok(DreamUsage {
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        })
    }

    async fn cleanup(
        &self,
        request: &DreamRequest,
        preparation: Option<&DreamPreparation>,
    ) -> Result<(), DreamFailure> {
        if let Some(preparation) = preparation {
            self.managed
                .archive_session(&preparation.session_id)
                .await
                .map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
            for file_id in &preparation.transcript_file_ids {
                self.host
                    .file_application()
                    .ok_or_else(|| {
                        DreamFailure::new("internal_error", "File application is unavailable")
                    })?
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
            let _ = self
                .managed
                .send_events(
                    &preparation.session_id,
                    SendEventsRequest {
                        events: vec![InboundEvent::UserInterrupt {
                            session_thread_id: None,
                        }],
                        user_profile_id: None,
                    },
                )
                .await;
        }
        self.cleanup(request, preparation).await
    }

    async fn validate_agent(&self, workspace_id: &str, agent_id: &str) -> Result<(), DreamFailure> {
        self.managed
            .validate_dream_agent(workspace_id, agent_id)
            .map_err(|error| DreamFailure::new("invalid_dream_agent", error.to_string()))
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
}
