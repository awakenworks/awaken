// @formal surface requirement=session.lifecycle.session_state
// @formal reason: This read-only Managed adapter delegates to the canonical Session application
// @formal reason: observation and performs only wire projection; it owns no durable fact or execution
// @formal reason: authority, and this link is not a formal-proof claim.
//! Awaken-only read projection for exact Session execution evidence.

use crate::common::scope::RequiredWorkspaceScope;
use awaken_session_execution_contract::SessionExecutionEvidenceSelection;
use std::sync::Arc;

use axum::Json;
use axum::extract::{Path, Query, State};

use super::{WireErr, session_projection_error};
use crate::state::ManagedState;

fn project_execution_evidence(
    observation: awaken_session_application::SessionExecutionObservation,
    selection: SessionExecutionEvidenceSelection,
) -> crate::awaken_extensions::SessionExecutionEvidence {
    let public_thread_id =
        crate::state::public_thread_id(&observation.session_id, &observation.session_id);
    let mut recovery = observation.recovery;
    let (operation_footprint, preparation, operation_inventory) =
        recovery.take_projection_details(selection);
    crate::awaken_extensions::SessionExecutionEvidence {
        configuration: observation
            .configuration
            .filter(|_| selection.include_inspection),
        environment: selection
            .include_inspection
            .then_some(observation.environment),
        artifact_capture: observation.artifact_capture,
        operation_footprint,
        operation_inventory,
        preparation,
        recovery: Some(recovery),
        session_id: observation.session_id,
        session_revision: observation.session_revision,
        agent_id: observation.agent_id,
        agent_revision: observation.agent_revision,
        application_origin: observation.application_origin,
        model: observation.model,
        execution_model_ref: observation.execution_model_ref,
        backend_ref: observation.backend_ref,
        execution: observation.execution,
        runs: observation
            .runs
            .into_iter()
            .map(
                |run| crate::awaken_extensions::SessionRunExecutionEvidence {
                    model_operations: if selection.include_inspection {
                        run.model_operations
                    } else {
                        Vec::new()
                    },
                    run_id: run.run_id,
                    idempotency_key: run.idempotency_key,
                    state: run.state,
                    tool_use_ids: run
                        .tool_uses
                        .into_iter()
                        .map(|tool_use| {
                            crate::project::managed_tool_event_id(
                                &public_thread_id,
                                &tool_use.source_message_id,
                                &tool_use.call_id,
                            )
                        })
                        .collect(),
                    model_requests: run.model_requests,
                },
            )
            .collect(),
    }
}

/// Read immutable backend/model selection together with committed Run/model
/// observations. The Session application enforces ownership and reads both
/// existing authorities; this adapter performs only wire projection.
pub async fn get_session_execution_evidence(
    State(state): State<Arc<ManagedState>>,
    Path(id): Path<String>,
    Query(query): Query<SessionExecutionEvidenceSelection>,
    RequiredWorkspaceScope(owner_scope): RequiredWorkspaceScope,
) -> Result<Json<crate::awaken_extensions::SessionExecutionEvidence>, WireErr> {
    let observation = state
        .roles
        .observation
        .session_execution_observation(&owner_scope, &awaken_session_types::SessionId::from(id))
        .await
        .map_err(session_projection_error)?;
    Ok(Json(project_execution_evidence(observation, query)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::{Id as RunId, RunState};

    #[test]
    fn projection_is_lossless_and_adds_no_execution_claim() {
        // Cause/effect decision table: P1 one application observation carries
        // frozen selection plus two Run facts; P2 one Run has no completed
        // model request. Effects: E1 every coordinate is preserved; E2 the
        // empty Run stays empty rather than being reported as executed. Rules:
        // R1=P1=>E1 and R2=P2=>E2.
        let hibernated = awaken_session_contract::SessionEnvironmentState::Hibernated {
            generation: awaken_session_contract::SandboxGeneration::new(
                &awaken_session_types::SessionId::from("session-1"),
                1,
                100,
                "environment",
                "image",
            ),
            checkpoint: awaken_provisioning_contract::SandboxCheckpointRef {
                id: "internal-object-id".into(),
                format: "awaken-fs-tar-v1".into(),
                digest: "archive-digest".into(),
                size_bytes: 4096,
                created_at_unix_ms: 5,
                expires_at_unix_ms: 60,
                environment_fingerprint: "environment".into(),
                base_image_fingerprint: "image".into(),
                excluded_mounts: vec![],
                suspend_effect_id: "internal-effect-id".into(),
            },
        };
        let mut observation = awaken_session_application::SessionExecutionObservation {
                configuration: None,
                environment: (&hibernated).into(),
                artifact_capture: Some(serde_json::from_value(serde_json::json!({
                    "workspace_id":"workspace","session_id":"session-1","revision":2,
                    "file_ids":["file-original"],"origin":{"run_id":"original-run","owner":"worker","epoch":7}
                })).unwrap()),
                recovery: awaken_session_execution_contract::SessionExecutionRecoveryObservation::NotProven {
                    reason: awaken_session_execution_contract::SessionExecutionRecoveryGap::NotTerminal,
                },
                session_id: "session-1".into(),
                session_revision: 5,
                agent_id: "agent-1".into(),
                agent_revision: Some(3),
                application_origin: Some(
                    awaken_session_configuration_contract::ApplicationOriginBinding::new(
                        "application.interaction:v1".into(),
                        "a".repeat(64),
                        "opaque-origin-proof".into(),
                    )
                    .unwrap(),
                ),
                model: "deepseek-v4-pro".into(),
                execution_model_ref: "deepseek-v4-pro".into(),
                backend_ref: Some("native".into()),
                execution: awaken_session_contract::SessionExecutionState::Running,
                runs: vec![
                    awaken_session_application::SessionRunExecutionObservation {
                        model_operations: vec![awaken_runtime_contract::model_inspection::ModelOperationInspection { ordinal: 1, provider: "provider-original".into(), model: "model-original".into(), backend: "native".into(), phase: "entered".into() }],                        run_id: RunId("run-observed".into()),
                        idempotency_key: Some("command-observed".into()),
                        state: RunState::Running,
                        tool_uses: vec![awaken_session_application::SessionToolUseObservation {
                            source_message_id: "run-observed-assistant-0".into(),
                            call_id: "tool-submit".into(),
                        }],
                        model_requests: vec![
                            awaken_session_application::SessionModelRequestObservation {
                                sequence: 11,
                                is_error: false,
                                retry_count: 0,
                            },
                        ],
                    },
                    awaken_session_application::SessionRunExecutionObservation {
                        model_operations: Vec::new(),                        run_id: RunId("run-unobserved".into()),
                        idempotency_key: None,
                        state: RunState::Running,
                        tool_uses: Vec::new(),
                        model_requests: Vec::new(),
                    },
                ],
            };
        // P8/P9 inspection is opt-in; legacy/default readers receive neither
        // a configuration snapshot nor model-operation fields. Explicit opt-in
        // retains safe baseline facts; it never resolves a current publication.
        let projected = project_execution_evidence(
            observation.clone(),
            SessionExecutionEvidenceSelection::default(),
        );
        assert_eq!(projected.backend_ref.as_deref(), Some("native"), "R1/E1");
        let legacy_wire = serde_json::to_value(&projected).unwrap();
        assert!(legacy_wire.get("configuration").is_none(), "P8");
        assert!(legacy_wire.get("environment").is_none());
        assert!(
            legacy_wire["runs"][0].get("model_operations").is_none(),
            "P8"
        );
        let inspected = project_execution_evidence(
            observation.clone(),
            SessionExecutionEvidenceSelection {
                include_inspection: true,
                ..Default::default()
            },
        );
        let wire = serde_json::to_value(&inspected).unwrap();
        assert_eq!(wire["environment"]["phase"], "hibernated");
        assert_eq!(
            wire["environment"]["generation_id"].as_str(),
            hibernated.generation().map(|generation| generation.id())
        );
        assert_eq!(wire["environment"]["checkpoint"]["size_bytes"], 4096);
        assert!(wire["environment"]["checkpoint"].get("id").is_none());
        assert!(
            wire["environment"]["checkpoint"]
                .get("suspend_effect_id")
                .is_none()
        );
        assert_eq!(
            inspected.runs[0].model_operations[0].model, "model-original",
            "P9 exact recorded route"
        );
        // P3 immutable source carrier is projected exactly; it is non-bearer
        // provenance and adds neither an execution claim nor a permission.
        assert_eq!(
            projected.application_origin.as_ref().unwrap().evidence(),
            "opaque-origin-proof",
            "P3"
        );
        // P3 catalog provenance is copied, never rebound to the current Run.
        assert_eq!(
            projected
                .artifact_capture
                .as_ref()
                .unwrap()
                .origin
                .as_ref()
                .unwrap()
                .run_id,
            "original-run",
            "P3 original publication reference"
        );
        assert_eq!(projected.runs[0].run_id.0, "run-observed", "R1/E1");
        assert_eq!(
            projected.runs[0].idempotency_key.as_deref(),
            Some("command-observed"),
            "R1/E1"
        );
        assert_eq!(
            projected.runs[0].tool_use_ids,
            [crate::project::managed_tool_event_id(
                &crate::state::public_thread_id("session-1", "session-1"),
                "run-observed-assistant-0",
                "tool-submit",
            )],
            "R1/E1"
        );
        assert_eq!(projected.runs[0].model_requests[0].sequence, 11, "R1/E1");
        assert!(projected.runs[1].model_requests.is_empty(), "R2/E2");

        // P4/P5: a caller selects an expanded projection, not permission.
        // Off -> no new field even when the owning observation has inventory;
        // On -> exact original ids, with no tool-settlement or retry claim.
        // P7 both selections preserve existing preparation/footprint fields and
        // closure coordinates while the Session language extracts the details.
        use awaken_session_execution_contract::{
            SessionExecutionOperationInventory as Inventory,
            SessionExecutionRecoveryObservation as Recovery, SessionExecutionToolOperation,
        };
        observation.execution = awaken_session_contract::SessionExecutionState::Terminated;
        let inventory = Inventory::Recorded {
            tools: vec![SessionExecutionToolOperation {
                thread_id: "session-1".into(),
                run_id: "run-observed".into(),
                operation_id: "original-operation".into(),
                tool_id: "original-tool".into(),
            }],
        };
        observation.recovery = Recovery::EffectsClosed {
            preparation: Some(
                awaken_session_execution_contract::SessionRecoveryPreparationReference {
                    session_id: "session-1".into(),
                    effect_id: "original-preparation".into(),
                    preparation_fingerprint: "original-fingerprint".into(),
                },
            ),
            operation_footprint: Some(
                awaken_session_execution_contract::SessionExecutionOperationFootprint::ResponseOnly,
            ),
            operation_inventory: Some(inventory.clone()),
            session_revision: 5,
            runtime_commit_cursor: 19,
            thread_ids: vec!["session-1".into()],
            closure_records: Vec::new(),
        };
        let standard = serde_json::to_value(project_execution_evidence(
            observation.clone(),
            SessionExecutionEvidenceSelection::default(),
        ))
        .unwrap();
        assert!(
            standard.get("operation_inventory").is_none(),
            "P4 legacy view unchanged"
        );
        assert!(
            standard["recovery"].get("operation_inventory").is_none(),
            "P4 no nested bypass"
        );
        assert_eq!(
            standard["preparation"]["effect_id"], "original-preparation",
            "P7"
        );
        assert_eq!(standard["operation_footprint"], "response_only", "P7");
        assert_eq!(standard["recovery"]["session_revision"], 5, "P7");
        assert_eq!(standard["recovery"]["runtime_commit_cursor"], 19, "P7");
        let expanded = project_execution_evidence(
            observation.clone(),
            SessionExecutionEvidenceSelection {
                include_operation_inventory: true,
                ..Default::default()
            },
        );
        assert_eq!(expanded.operation_inventory, Some(inventory), "P5");
        assert_eq!(
            expanded.execution, observation.execution,
            "P5 read cannot change execution"
        );
        // P6 missing provenance stays a typed negative observation; neither
        // the selector nor physical closure upgrades business effects.
        if let Recovery::EffectsClosed {
            operation_inventory,
            ..
        } = &mut observation.recovery
        {
            *operation_inventory = Some(Inventory::NotProven {
                reason:
                    awaken_session_execution_contract::SessionExecutionRecoveryGap::HistoryMissing,
            });
        }
        assert!(
            matches!(
                project_execution_evidence(
                    observation,
                    SessionExecutionEvidenceSelection {
                        include_operation_inventory: true,
                        ..Default::default()
                    }
                )
                .operation_inventory,
                Some(Inventory::NotProven {
                    reason:
                        awaken_session_execution_contract::SessionExecutionRecoveryGap::HistoryMissing
                })
            ),
            "P6"
        );
    }

    #[test]
    fn operation_inventory_query_is_explicit_and_closed() {
        // Q1 absent/false -> original projection; Q2 true -> expanded read;
        // Q3 malformed Boolean or unknown selector -> rejection. Parsing
        // dispatches no operation and grants no recovery permission.
        for (query, expected) in [
            ("", Some(false)),
            ("?include_operation_inventory=false", Some(false)),
            ("?include_operation_inventory=true", Some(true)),
            ("?include_inspection=true", Some(false)),
            ("?include_inspection=allow", None),
            ("?include_operation_inventory=allow", None),
            ("?safe_to_retry=true", None),
        ] {
            let uri = format!("/v1/awaken/sessions/s/execution-evidence{query}")
                .parse()
                .unwrap();
            let observed = Query::<SessionExecutionEvidenceSelection>::try_from_uri(&uri);
            match expected {
                Some(value) => assert_eq!(
                    observed.unwrap().include_operation_inventory,
                    value,
                    "Q1/Q2 {query}"
                ),
                None => assert!(observed.is_err(), "Q3 {query}"),
            }
        }
    }
}
