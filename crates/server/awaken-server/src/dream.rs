//! Production dream worker composed from existing authorities.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use awaken_memory_store::{Memory, MemoryRepository};
use awaken_protocol_managed::ResourceCatalog;
use awaken_protocol_managed::resource_plane::{
    ConfigVersion, MemoryStoreConfigVersion, MemoryStoreDefinition, MemoryStoreId, ResourceState,
    ResourceTimestamps,
};
use awaken_protocol_managed::types::{DreamUsage, InboundEvent, SendEventsRequest};
use awaken_protocol_managed::{
    ApplicationSessionContribution, ApplicationSessionContributionPort, ApplicationSessionInput,
    DreamCancellation, DreamFailure, DreamPreparation, DreamRequest, DreamWorker, ManagedState,
};
use awaken_runtime_host::{
    MemoryWriteConsistency, MountAccess, MountLifetime, MountRequirement, MountSource,
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

#[derive(Clone)]
struct PreparedResources {
    session_id: String,
    snapshot_memory_store_id: String,
    writer_lease: ExclusiveMemoryStoreWriterLease,
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
    prepared: Mutex<BTreeMap<String, PreparedResources>>,
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
            prepared: Mutex::new(BTreeMap::new()),
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
    ) -> Result<Vec<(String, Vec<u8>)>, DreamFailure> {
        let mut exports = Vec::with_capacity(request.session_ids.len());
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
            self.host
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
        }
        Ok(exports)
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
                        {"name":"grep", "enabled":true, "permission_policy":{"type":"always_allow"}}
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

    async fn release_result(&self, request: &DreamRequest) {
        let resources = self.prepared.lock().unwrap().remove(&request.job_id);
        if let Some(resources) = resources {
            resources.writer_lease.release(self.catalog.as_ref());
            let _ = self
                .memory
                .purge_store(&resources.snapshot_memory_store_id)
                .await;
        }
    }
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
            let session_id = if session_exists {
                expected_session_id
            } else {
                let transcripts = self.export_transcripts(request).await?;
                self.make_session(request, &snapshot_id, &result_id, transcripts)
                    .await?
            };
            self.prepared.lock().unwrap().insert(
                request.job_id.clone(),
                PreparedResources {
                    session_id: session_id.clone(),
                    snapshot_memory_store_id: snapshot_id,
                    writer_lease: ExclusiveMemoryStoreWriterLease {
                        workspace_id: request.workspace_id.clone(),
                        result_memory_store_id: result_id.clone(),
                    },
                },
            );
            return Ok(DreamPreparation {
                result_memory_store_id: result_id,
                session_id,
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
        let transcripts = self.export_transcripts(request).await?;
        let session_id = self
            .make_session(request, &snapshot_id, &result_id, transcripts)
            .await?;
        self.prepared.lock().unwrap().insert(
            request.job_id.clone(),
            PreparedResources {
                session_id: session_id.clone(),
                snapshot_memory_store_id: snapshot.snapshot_memory_store_id,
                writer_lease: ExclusiveMemoryStoreWriterLease {
                    workspace_id: request.workspace_id.clone(),
                    result_memory_store_id: result_id.clone(),
                },
            },
        );
        Ok(DreamPreparation {
            result_memory_store_id: result_id,
            session_id,
        })
    }

    async fn execute(
        &self,
        request: &DreamRequest,
        preparation: &DreamPreparation,
        cancellation: DreamCancellation,
    ) -> Result<DreamUsage, DreamFailure> {
        if cancellation.is_canceled() {
            self.release_result(request).await;
            return Ok(DreamUsage::default());
        }
        let content = vec![awaken_agent_contract::agent::content::ContentBlock::text(
            "Consolidate the frozen memory and Session evidence now.",
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
        let session = self.managed.get_session(&preparation.session_id).ok();
        let _ = self.managed.archive_session(&preparation.session_id).await;
        self.release_result(request).await;
        run.map_err(|error| DreamFailure::new("internal_error", error.to_string()))?;
        let usage = session.map(|session| session.usage).unwrap_or_default();
        Ok(DreamUsage {
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
        })
    }

    async fn cancel(&self, session_id: Option<&str>) -> Result<(), DreamFailure> {
        if let Some(session_id) = session_id {
            let _ = self
                .managed
                .send_events(
                    session_id,
                    SendEventsRequest {
                        events: vec![InboundEvent::UserInterrupt {
                            session_thread_id: None,
                        }],
                        user_profile_id: None,
                    },
                )
                .await;
            let _ = self.managed.archive_session(session_id).await;
            let resources = {
                let mut prepared = self.prepared.lock().unwrap();
                let job_id = prepared.iter().find_map(|(job_id, resources)| {
                    (resources.session_id == session_id).then(|| job_id.clone())
                });
                job_id.and_then(|job_id| prepared.remove(&job_id))
            };
            if let Some(resources) = resources {
                resources.writer_lease.release(self.catalog.as_ref());
                let _ = self
                    .memory
                    .purge_store(&resources.snapshot_memory_store_id)
                    .await;
            }
        }
        Ok(())
    }
}
