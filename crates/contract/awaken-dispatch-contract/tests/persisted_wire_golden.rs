//! Golden bytes for every Session value a durable dispatch row or Resource
//! transport persists (S3b step 0).
//!
//! S3b moves these types out of the dispatch contract into the Session
//! contracts. The stored rows, completion tombstones and transport bodies must
//! not change while they move, so each value is pinned here twice: encoding
//! the typed value yields exactly the literal, and decoding the literal (a row
//! written before the move) yields exactly the typed value.
//!
//! | Rule | Persisted value | Effect pinned |
//! |---|---|---|
//! | G1 | `SessionRunCommandFingerprint` of a fixed command | the `session-command-v1:sha256:` digest |
//! | G2 | `RunDispatch` with fingerprint, `SupersedePrior`, both envelopes | complete row JSON, decode of the old row, both identities |
//! | G3 | Session runtime projection with an MCP credential | projection JSON inside the envelope and its decode |
//! | G4 | Session resource manifest | manifest JSON inside the envelope and its decode |
//! | G5 | each `ResourceOperationFence` arm | tagged transport JSON and its decode |

use awaken_agent_contract::agent::message::{Id as MessageId, Message, Role};
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_agent_contract::agent::thread::Id as ThreadId;
use awaken_agent_contract::credential::{
    CredentialAccess, CredentialExecutionPolicy, CredentialMaterialSource, CredentialRef,
    CredentialUsage, ModelExposurePolicy, PlaintextBoundary, PlaintextHolder,
};
use awaken_dispatch_contract::{
    RunClaim, RunDispatch, SessionResourceEnvelope, SessionRuntimeEnvelope,
};
use awaken_runtime_contract::activation::RunActivation;
use awaken_runtime_contract::resolved::{CatalogFingerprint, ModelBinding, ResolvedSpec};
use awaken_runtime_contract::snapshot::{
    AgentId, ExecutableAgentSnapshot, ExecutableAgentSnapshotId,
};
use awaken_session_configuration_contract::SessionRuntimeProjection;
use awaken_session_configuration_contract::resource::{
    SessionRealizationLease, SessionResourceManifest,
};
use awaken_session_execution_contract::{AdmitSessionRun, SessionRunExecutionRequirements};
use awaken_session_realization_contract::ResourceOperationFence;
use awaken_session_types::{SessionRunCommandFingerprint, SessionRunReplacement};

fn command() -> AdmitSessionRun {
    AdmitSessionRun {
        session_id: "session-golden".into(),
        agent_id: "agent-golden".into(),
        operation_id: "operation-golden".into(),
        run_id: RunId("run-golden".into()),
        messages: vec![Message::text(
            MessageId("message-golden".into()),
            Role::User,
            "persist me",
        )],
        data_subject_id: Some("subject-golden".into()),
        traceparent: None,
        execution_requirements: SessionRunExecutionRequirements::default(),
        replacement: SessionRunReplacement::SupersedePrior,
        private_task_input: None,
    }
}

fn command_fingerprint() -> SessionRunCommandFingerprint {
    command().command_fingerprint()
}

fn activation() -> RunActivation {
    RunActivation::new(
        RunId("run-golden".into()),
        ThreadId("session-golden".into()),
        ExecutableAgentSnapshot {
            id: ExecutableAgentSnapshotId("snapshot-golden".into()),
            metadata: Default::default(),
            root_agent_id: AgentId("agent-golden".into()),
            resolved_spec: ResolvedSpec {
                model_candidates: Vec::new(),
                catalog_fingerprint: CatalogFingerprint("catalog-golden".into()),
                instructions: "be exact".into(),
                max_steps: Some(4),
                delegation_limits: Default::default(),
                model_binding: awaken_runtime_contract::resolved::ResolvedModelCandidate::host(
                    ModelBinding::parse("provider", "model", "acp:golden")
                        .expect("valid model binding"),
                ),
                tool_descriptors: Vec::new(),
                plugin_ids: Vec::new(),
                plugin_config: Default::default(),
                context_policy: Default::default(),
                tool_presentation: Default::default(),
            },
            fingerprint: CatalogFingerprint("catalog-golden".into()),
        },
        vec![Message::text(
            MessageId("message-golden".into()),
            Role::User,
            "persist me",
        )],
    )
}

fn workload() -> PlaintextHolder {
    PlaintextHolder::new(PlaintextBoundary::Workload, "workload-golden")
}

fn mcp_credential() -> CredentialAccess {
    CredentialAccess::new(
        CredentialRef {
            id: "mcp-credential-golden".into(),
            revision: 3,
        },
        CredentialMaterialSource::ControlPlaneReference,
        CredentialUsage::HttpHeader {
            name: "Authorization".into(),
            scheme: Some("Bearer".into()),
        },
        CredentialExecutionPolicy::exact(workload(), ModelExposurePolicy::Forbidden),
    )
    .with_target(awaken_agent_contract::credential::CredentialTarget::new(
        awaken_agent_contract::credential::CredentialPurpose::McpAuthorization,
        "https://mcp.golden.test/",
    ))
}

fn environment() -> awaken_session_configuration_contract::EnvironmentSnapshot {
    awaken_session_configuration_contract::EnvironmentSnapshot {
        environment_id: "environment-golden".into(),
        revision: awaken_environment_contract::EnvironmentRevision(2),
        self_hosted: false,
        config_fingerprint: awaken_session_configuration_contract::EnvironmentFingerprint(
            "environment-golden@2".into(),
        ),
        sandbox: Default::default(),
        sandbox_provisioning: Default::default(),
        idle_retention: Default::default(),
        packages: Default::default(),
        prepared_image: None,
        network: awaken_session_configuration_contract::SessionNetworkPolicy::Unrestricted,
        credential_realization: awaken_credential_contract::CredentialRealizationProfile {
            inference_holder: workload(),
            mcp_holder: workload(),
            resource_holder: workload(),
        },
    }
}

fn mcp_stage() -> awaken_session_configuration_contract::mcp::StageMcpAttachment {
    let target = awaken_agent_contract::McpTarget::parse_http("https://mcp.golden.test")
        .expect("valid MCP target");
    awaken_session_configuration_contract::mcp::StageMcpAttachment {
        workspace_id: "workspace-golden".into(),
        generation: awaken_session_configuration_contract::mcp::McpGenerationRef::from_persisted(
            "session-golden".into(),
            awaken_session_configuration_contract::mcp::McpGeneration::of_bare_target(
                awaken_session_configuration_contract::mcp::McpAttachmentId("mcp-golden".into()),
                1,
                &target,
            ),
            "worker-golden".into(),
            1,
            10_000,
        )
        .expect("MCP generation fence epoch is nonzero"),
        realization_id: "realization-golden".into(),
        stage_idempotency_key: "stage-golden".into(),
        name: "mcp-golden".into(),
        target,
        prompts_as_skills: false,
        credential: Some(mcp_credential()),
        selected_plaintext_holder: Some(workload()),
    }
}

fn runtime_envelope() -> SessionRuntimeEnvelope {
    SessionRuntimeEnvelope::new(
        SessionRuntimeProjection {
            environment: environment(),
            tools: None,
            mcp_stages: Some(vec![mcp_stage()]),
        }
        .encode()
        .expect("projection encodes"),
    )
}

fn repository_input() -> serde_json::Value {
    serde_json::json!({
        "binding_id": "repository-binding",
        "source": {
            "kind": "repository",
            "repository_id": "repository",
            "config": {
                "repository_id": "repository",
                "version": 1,
                "remote_url": "https://git.invalid/repository.git",
                "initial_branch": "main"
            }
        },
        "mount_path": "/workspace/repository",
        "access": "read_write"
    })
}

fn resource_manifest() -> SessionResourceManifest {
    SessionResourceManifest::at_revision(
        "workspace-golden",
        5,
        serde_json::from_value(serde_json::json!({ "inputs": [repository_input()] }))
            .expect("valid resolved resources"),
    )
}

fn resource_envelope() -> SessionResourceEnvelope {
    let manifest = resource_manifest();
    SessionResourceEnvelope::at_revision(
        manifest.workspace_id.clone(),
        manifest.revision,
        manifest.encode_resources().expect("manifest encodes"),
    )
}

fn dispatch() -> RunDispatch {
    RunDispatch::new(activation())
        .for_session(ThreadId("session-golden".into()))
        .with_session_command_fingerprint(command_fingerprint())
        .with_session_run_replacement(SessionRunReplacement::SupersedePrior)
        .with_session_command_identity()
        .with_session_resources(resource_envelope())
        .with_session_runtime(runtime_envelope())
}

fn lease() -> SessionRealizationLease {
    SessionRealizationLease {
        owner: "worker-golden".into(),
        runtime_incarnation: "worker-golden/boot".into(),
        epoch: 7,
        expires_at_unix_ms: 20_000,
    }
}

fn fences() -> Vec<(&'static str, ResourceOperationFence)> {
    let session = awaken_session_types::SessionId::from("session-golden");
    vec![
        (
            "run",
            ResourceOperationFence::from(RunClaim {
                run_id: RunId("run-golden".into()),
                owner: "worker-golden".into(),
                epoch: 4,
            }),
        ),
        (
            "session_materialization",
            ResourceOperationFence::SessionMaterialization(
                awaken_session_realization_contract::SessionResourceMaterializationIntent::try_new(
                    "session-golden",
                    5,
                    serde_json::from_value(repository_input()).expect("valid resolved input"),
                    lease(),
                )
                .expect("valid materialization intent"),
            ),
        ),
        (
            "repository_publication",
            ResourceOperationFence::RepositoryPublication {
                command: Box::new(
                    awaken_session_contract::SessionRepositoryPublicationCommand {
                        session_id: session.clone(),
                        effect_id: "publication-effect".into(),
                        intent: serde_json::from_value(serde_json::json!({
                            "input": repository_input(),
                            "expectation": {
                                "branch": "awf/publication",
                                "commit": "0123456789abcdef0123456789abcdef01234567"
                            }
                        }))
                        .expect("valid publication intent"),
                    },
                ),
                lease: lease(),
            },
        ),
        (
            "checkpoint_release",
            ResourceOperationFence::CheckpointRelease(
                // Fixed legacy wire retains the original opaque id/start/lease;
                // the golden bytes must not be regenerated by a new constructor.
                serde_json::from_value(serde_json::json!({
                    "effect_id": "checkpoint-effect",
                    "activity_epoch": 3,
                    "started_at_unix_ms": 12_000,
                    "realization": lease(),
                }))
                .expect("unchanged legacy checkpoint operation"),
            ),
        ),
        (
            "terminal",
            ResourceOperationFence::Terminal(
                awaken_session_contract::SessionTerminalCleanupEffect::new(
                    awaken_session_contract::SessionCleanupCommand::new(
                        &session,
                        "workspace-golden",
                        "cleanup-root",
                    ),
                    lease(),
                ),
            ),
        ),
    ]
}

/// Read one persisted literal written by the pre-move build.
fn golden(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/persisted_wire/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read {path}: {error}"))
        .trim_end()
        .to_string()
}

const COMMAND_FINGERPRINT: &str =
    "session-command-v1:sha256:e45c3a62d2b5d6f3d3fd0027128cd7f8c1effd6a3a00e6f9c78fc38d0aea14fb";
const CANONICAL_FINGERPRINT: &str =
    "sha256:ec7261f5d0f79cb64e5dd60866b796aeadeea4ac44a435c6e5b052010f722167";

#[test]
fn session_command_fingerprint_bytes_are_pinned() {
    // G1: the digest tuple, its order and the version prefix are unchanged.
    // Import-only ownership migration adds no runtime condition combination:
    // compilation proves every consumer names SessionTypes directly; G1/G2
    // keep the original bytes and replay identities as independent oracles.
    // Do not regenerate fixtures to accept an import change.
    let fingerprint = command_fingerprint();
    assert_eq!(fingerprint.as_str(), COMMAND_FINGERPRINT, "G1 encode");
    assert!(fingerprint.is_current(), "G1 current version");
    let decoded: SessionRunCommandFingerprint =
        serde_json::from_value(serde_json::Value::String(COMMAND_FINGERPRINT.into()))
            .expect("G1 decode");
    assert_eq!(decoded, fingerprint, "G1 decode");
}

#[test]
fn run_dispatch_row_bytes_and_identities_are_pinned() {
    // G2: the full row, its decode by a later build, and both identities a
    // live row or completion tombstone compares.
    let row = golden("run_dispatch.json");
    let dispatch = dispatch();
    assert_eq!(
        serde_json::to_string(&dispatch).expect("G2 encode"),
        row,
        "G2 encode"
    );
    let decoded: RunDispatch = serde_json::from_str(&row).expect("G2 decode old row");
    assert_eq!(decoded, dispatch, "G2 decode old row");
    assert_eq!(
        decoded.admission_fingerprint(),
        COMMAND_FINGERPRINT,
        "G2 tombstone identity"
    );
    assert!(dispatch.same_admission_dispatch(&decoded), "G2 live replay");
    assert_eq!(
        decoded.canonical_fingerprint(),
        CANONICAL_FINGERPRINT,
        "G2 canonical identity"
    );
    assert!(
        decoded.session_run_replacement.supersedes_prior(),
        "G2 replacement intent"
    );
}

#[test]
fn session_runtime_projection_bytes_are_pinned() {
    // G3: the projection inside the runtime envelope, with an MCP credential.
    let projection = golden("session_runtime_projection.json");
    assert_eq!(runtime_envelope().projection_json, projection, "G3 encode");
    let decoded = SessionRuntimeProjection::decode(&projection).expect("G3 decode");
    assert_eq!(decoded.environment, environment(), "G3 environment");
    assert_eq!(decoded.tools, None, "G3 tools");
    assert_eq!(decoded.mcp_stages, Some(vec![mcp_stage()]), "G3 MCP stages");
}

#[test]
fn session_resource_manifest_bytes_are_pinned() {
    // G4: the manifest inside the resource envelope and its generation.
    let resources = golden("session_resource_manifest.json");
    let envelope = resource_envelope();
    assert_eq!(envelope.resolved_resources_json, resources, "G4 encode");
    assert_eq!(envelope.workspace_id, "workspace-golden", "G4 Workspace");
    assert_eq!(envelope.resource_revision, 5, "G4 revision");
    let decoded = SessionResourceEnvelope::at_revision("workspace-golden", 5, resources)
        .decode_with(SessionResourceManifest::decode_resources)
        .expect("G4 decode");
    assert_eq!(decoded, resource_manifest(), "G4 decode");
}

#[test]
fn resource_operation_fence_bytes_are_pinned_for_every_arm() {
    // G5: the tagged transport form never names the Rust type, so moving the
    // enum keeps every arm's bytes.
    let fences = fences();
    assert_eq!(fences.len(), 5, "G5 covers every arm");
    for (arm, fence) in fences {
        let wire = golden(&format!("fence_{arm}.json"));
        assert_eq!(
            serde_json::to_string(&fence).expect("G5 encode"),
            wire,
            "G5 encode {arm}"
        );
        let decoded: ResourceOperationFence = serde_json::from_str(&wire).expect("G5 decode");
        assert_eq!(decoded, fence, "G5 decode {arm}");
    }
}
