//! Explicit transport for complete product-authored Session creation.

use std::collections::BTreeMap;

use awaken_credential_contract::CredentialRef;
use awaken_provisioning_contract::{
    EnvVar, MountRequirement, RepositoryPublicationExpectation, RepositoryPublicationReceipt,
};
use awaken_session_contract::{
    McpAttachmentOrigin, McpTarget, SessionNetworkPolicy, SessionRunExecutionRequirements,
    SessionToolConfiguration,
};
use axum::Router;
use axum::handler::Handler;
use axum::routing::post;
use serde::{Deserialize, Serialize};

/// One already-authorized, secret-free MCP candidate supplied by a product.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionMcpAttachment {
    pub name: String,
    pub target: McpTarget,
    #[serde(default)]
    pub prompts_as_skills: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub published_credential: Option<CredentialRef>,
    pub origin: McpAttachmentOrigin,
}

/// One exact Repository selected by the product for this Session. The
/// credential is a secret-free Vault reference; Session admission turns it into
/// the existing recipient-bound execution pin before persistence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRepository {
    /// Caller-owned binding identity retained in the canonical Session input.
    /// Open still owns the internal Session-scoped Repository definition id.
    /// `None` is the historical private wire and is lowered to its former
    /// deterministic Session/index-derived binding by the adapter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_id: Option<String>,
    pub remote_url: String,
    pub mount_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<CredentialRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initial_commit: Option<String>,
}

/// Product intent projected into Open's neutral immutable mutation policy.
/// The private extension deliberately has no ordinary Managed variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfiledSessionMode {
    WorkUnit,
    Interactive,
}

impl ProfiledSessionMode {
    #[must_use]
    pub const fn mutation_policy(self) -> awaken_session_contract::SessionMutationPolicy {
        match self {
            Self::WorkUnit => awaken_session_contract::SessionMutationPolicy::Frozen,
            Self::Interactive => awaken_session_contract::SessionMutationPolicy::FileResources,
        }
    }
}

/// Complete, strongly typed Session input delivered to Awaken's sole profiled
/// Session composer. Secret material is forbidden; mounts carry only references.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionCreate {
    pub session_id: String,
    pub mode: ProfiledSessionMode,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_id: Option<String>,
    #[serde(default)]
    pub mounts: Vec<MountRequirement>,
    #[serde(default)]
    pub env: Vec<EnvVar>,
    #[serde(default)]
    pub prompts: Vec<String>,
    /// Complete Session-local resource inputs. Open resolves these together
    /// with published Agent defaults before the original root insert.
    #[serde(default)]
    pub resource_inputs: Vec<awaken_session_contract::SessionInputAttachment>,
    #[serde(default)]
    pub mcp_attachments: Vec<ProfiledSessionMcpAttachment>,
    #[serde(default)]
    pub repositories: Vec<ProfiledSessionRepository>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub network_restriction: Option<SessionNetworkPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<SessionToolConfiguration>,
}

/// Minimal projection returned after the canonical Session root is durable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionCreated {
    pub id: String,
    pub metadata: BTreeMap<String, String>,
}

/// Complete immutable input for one Run of an existing product-authored
/// Session. The Session identity is owned by the path, so the body cannot carry
/// a second coordinate that could disagree with it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRunSubmit {
    pub agent_id: String,
    pub operation_id: String,
    pub run_id: awaken_agent_contract::agent::run::Id,
    pub messages: Vec<awaken_agent_contract::agent::message::Message>,
    pub execution_requirements: SessionRunExecutionRequirements,
}

/// Stable acknowledgement that the canonical Session Run is durably admitted
/// and linked for execution. It is intentionally not a second Run status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRunReceipt {
    pub session_id: String,
    pub run_id: awaken_agent_contract::agent::run::Id,
}

/// Exact Repository selector and Git coordinate supplied by the product at the
/// terminal release boundary. The adapter resolves `binding_id` against the
/// Session root; callers cannot supply or reconstruct a `ResolvedInput`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRepositoryPublication {
    pub binding_id: String,
    pub expectation: RepositoryPublicationExpectation,
}

/// Private product-owned Session release. Absence of `repository_publication`
/// preserves the established terminal cleanup behavior exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRelease {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_publication: Option<ProfiledSessionRepositoryPublication>,
}

/// Protocol projection of the canonical Session-owned publication receipt.
/// `binding_id` correlates the result to the product's frozen input; the receipt
/// itself remains owned by the neutral provisioning contract.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionRepositoryPublished {
    pub binding_id: String,
    pub receipt: RepositoryPublicationReceipt,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProfiledSessionReleased {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repository_publication: Option<ProfiledSessionRepositoryPublished>,
}

/// Mount a caller-supplied lowering handler under Awaken's extension namespace.
pub fn profiled_session_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions", post(handler))
}

/// Mount the private profiled-Run adapter beside Session creation. The handler
/// lowers into the existing Session application admission path and owns no Run
/// state, dispatch queue, or compatibility event path.
pub fn profiled_session_run_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/runs", post(handler))
}

/// Mount the private terminal-release adapter beside the create extension. It
/// projects the existing Session cleanup operation and owns no release state.
pub fn profiled_session_release_router<H, T, S>(handler: H) -> Router<S>
where
    H: Handler<T, S>,
    T: 'static,
    S: Clone + Send + Sync + 'static,
{
    Router::new().route("/v1/awaken/sessions/{id}/release", post(handler))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_agent_contract::agent::run::Id as RunId;
    use awaken_provisioning_contract::{
        EnvValue, EnvVisibility, MountAccess, MountLifetime, MountSource,
    };

    #[test]
    fn profiled_session_wire_preserves_typed_secret_references() {
        // Cause/effect decision table:
        // R1 typed Secret mount + Repository credential reference + inline
        // environment -> exact round trip;
        // R2 unknown wire field -> reject before the application handler;
        // R3 missing mode -> reject rather than silently selecting ordinary
        // Managed mutation; R4/R5 map the two closed product modes to Open's
        // immutable Frozen/FileResources policies; R6 a historical Repository
        // without `binding_id` remains distinguishable as None and round-trips
        // without changing its idempotency payload; R7 an explicitly empty
        // binding remains Some("") so the application can reject it rather than
        // treating it as the historical omission.
        // The protocol never accepts plaintext credential material.
        let request = ProfiledSessionCreate {
            session_id: "session-a".into(),
            mode: ProfiledSessionMode::WorkUnit,
            agent_id: "agent-a".into(),
            source_revision: Some(2),
            environment_id: Some("environment-a".into()),
            mounts: vec![MountRequirement {
                mount_id: "git-credential".into(),
                source: MountSource::Secret {
                    reference: "credential-source-a@2".into(),
                    content_hash: None,
                },
                mount_path: "/run/awaken/git-credential".into(),
                access: MountAccess::ReadOnly,
                lifetime: MountLifetime::PerRun,
                required: true,
            }],
            env: vec![EnvVar {
                name: "GIT_TERMINAL_PROMPT".into(),
                value: EnvValue::Inline { value: "0".into() },
                visibility: EnvVisibility::Process,
            }],
            prompts: Vec::new(),
            resource_inputs: Vec::new(),
            mcp_attachments: Vec::new(),
            repositories: vec![ProfiledSessionRepository {
                binding_id: Some("repository-input-a".into()),
                remote_url: "https://github.com/acme/repository".into(),
                mount_path: "/workspace/repository".into(),
                credential: Some(CredentialRef {
                    id: "credential-source-a".into(),
                    revision: 7,
                }),
                initial_branch: Some("main".into()),
                initial_commit: None,
            }],
            network_restriction: Some(SessionNetworkPolicy::Unrestricted),
            title: None,
            metadata: BTreeMap::new(),
            tools: None,
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<ProfiledSessionCreate>(encoded.clone()).unwrap(),
            request,
            "R1"
        );
        assert_eq!(
            encoded["repositories"][0]["credential"]["revision"], 7,
            "R1 exact Repository credential pin"
        );
        let mut historical = encoded.clone();
        historical["repositories"][0]
            .as_object_mut()
            .unwrap()
            .remove("binding_id");
        let historical_decoded =
            serde_json::from_value::<ProfiledSessionCreate>(historical.clone()).unwrap();
        assert_eq!(
            historical_decoded.repositories[0].binding_id, None,
            "R6 omitted historical binding"
        );
        assert_eq!(
            serde_json::to_value(historical_decoded).unwrap(),
            historical,
            "R6 historical fingerprint shape"
        );
        let mut explicit_empty = encoded.clone();
        explicit_empty["repositories"][0]["binding_id"] = serde_json::json!("");
        assert_eq!(
            serde_json::from_value::<ProfiledSessionCreate>(explicit_empty)
                .unwrap()
                .repositories[0]
                .binding_id
                .as_deref(),
            Some(""),
            "R7 explicit empty is not historical omission"
        );
        let mut missing_mode = encoded.clone();
        missing_mode.as_object_mut().unwrap().remove("mode");
        assert!(
            serde_json::from_value::<ProfiledSessionCreate>(missing_mode).is_err(),
            "R3"
        );
        assert_eq!(
            ProfiledSessionMode::WorkUnit.mutation_policy(),
            awaken_session_contract::SessionMutationPolicy::Frozen,
            "R4"
        );
        assert_eq!(
            ProfiledSessionMode::Interactive.mutation_policy(),
            awaken_session_contract::SessionMutationPolicy::FileResources,
            "R5"
        );
        let mut unknown = encoded;
        unknown["credential"] = serde_json::json!("plaintext");
        assert!(
            serde_json::from_value::<ProfiledSessionCreate>(unknown).is_err(),
            "R2"
        );
    }

    #[test]
    fn profiled_release_wire_follows_the_publication_decision_table() {
        // Causes: C1 publication is absent/present; C2 binding, branch, desired
        // commit, and optional expected-prior commit are complete; C3 the wire
        // adds an unknown field. Effects: E1
        // no-publication release retains the exact empty object; E2 a present
        // publication round-trips one typed selector/expectation; E3 missing or
        // unknown authority is rejected before the handler. Rules: W1 !C1=>E1;
        // W2 C1+C2+!C3=>E2; W3 C1+!C2=>E3; W4 C3=>E3.
        let legacy = ProfiledSessionRelease::default();
        assert_eq!(
            serde_json::to_value(&legacy).unwrap(),
            serde_json::json!({}),
            "W1"
        );

        let request = ProfiledSessionRelease {
            repository_publication: Some(ProfiledSessionRepositoryPublication {
                binding_id: "repository-input-a".into(),
                expectation: RepositoryPublicationExpectation {
                    branch: "awf/work-unit-a".into(),
                    commit: "0123456789abcdef0123456789abcdef01234567".into(),
                    expected_prior_commit: None,
                },
            }),
        };
        let encoded = serde_json::to_value(&request).unwrap();
        assert_eq!(
            serde_json::from_value::<ProfiledSessionRelease>(encoded.clone()).unwrap(),
            request,
            "W2"
        );
        let mut update = request.clone();
        update
            .repository_publication
            .as_mut()
            .unwrap()
            .expectation
            .expected_prior_commit = Some("1111111111111111111111111111111111111111".into());
        let update_wire = serde_json::to_value(&update).unwrap();
        assert_eq!(
            update_wire["repository_publication"]["expectation"]["expected_prior_commit"],
            serde_json::json!("1111111111111111111111111111111111111111"),
            "W2 explicit CAS prior"
        );

        let mut missing = encoded.clone();
        missing["repository_publication"]
            .as_object_mut()
            .unwrap()
            .remove("binding_id");
        assert!(
            serde_json::from_value::<ProfiledSessionRelease>(missing).is_err(),
            "W3"
        );

        let mut unknown = encoded;
        unknown["publication_bypass"] = serde_json::json!(true);
        assert!(
            serde_json::from_value::<ProfiledSessionRelease>(unknown).is_err(),
            "W4"
        );
    }

    #[test]
    fn profiled_run_wire_preserves_the_canonical_command_and_rejects_parallel_identity() {
        // Cause/effect graph: C1 exact Agent/operation/Run/Message identities,
        // roles, content, and restrictions; C2 a body-level Session identity or
        // other unknown field; C3 an unknown nested execution requirement.
        // Effects: E1 C1 round-trips byte-for-byte; E2 C2/C3 fail before the
        // handler; E3 the receipt exposes only path Session plus admitted Run.
        // Rules R1/R4/R9/R10 require E1, R5/R7 require E2, and W1 covers E3.
        let encoded = serde_json::json!({
            "agent_id": "coding-agent",
            "operation_id": "issue-42:primary",
            "run_id": "flow-run-42-primary",
            "messages": [{
                "id": "flow-message-42",
                "role": "User",
                "content": [{"type": "text", "text": "Implement the accepted Issue"}]
            }],
            "execution_requirements": {
                "tool_capability_narrowing": "deny_all",
                "required_worker_capabilities": ["awaken.flow.coding.v1"]
            }
        });
        let request = serde_json::from_value::<ProfiledSessionRunSubmit>(encoded.clone()).unwrap();
        assert_eq!(serde_json::to_value(&request).unwrap(), encoded, "R1/R4/R9");

        let mut parallel_session_identity = encoded.clone();
        parallel_session_identity["session_id"] = serde_json::json!("other-session");
        assert!(
            serde_json::from_value::<ProfiledSessionRunSubmit>(parallel_session_identity).is_err(),
            "R5/R7 body cannot duplicate path identity"
        );
        let mut unknown_requirement = encoded;
        unknown_requirement["execution_requirements"]["fallback_to_local"] =
            serde_json::json!(true);
        assert!(
            serde_json::from_value::<ProfiledSessionRunSubmit>(unknown_requirement).is_err(),
            "R7 nested requirements remain closed"
        );

        let receipt = ProfiledSessionRunReceipt {
            session_id: "profiled-session-42".into(),
            run_id: RunId("flow-run-42-primary".into()),
        };
        assert_eq!(
            serde_json::from_value::<ProfiledSessionRunReceipt>(
                serde_json::to_value(&receipt).unwrap()
            )
            .unwrap(),
            receipt,
            "W1"
        );
    }

    #[tokio::test]
    async fn profiled_run_route_inventory_has_one_post_only_owner() {
        // Route causes: C1 exact path+POST; C2 exact path+another method; C3 a
        // sibling path. Effects: E1 only C1 reaches the leaf; E2 C2 is 405; E3
        // C3 is 404. Rules I1=C1=>E1, I2=C2=>E2, I3=C3=>E3. Workspace rewriting
        // is owned by Coordinator, so this crate registers no prefixed copy.
        use axum::Json;
        use axum::body::Body;
        use axum::extract::Path;
        use axum::http::{Request, StatusCode};
        use tower::ServiceExt as _;

        async fn accept(
            Path(session_id): Path<String>,
            Json(body): Json<ProfiledSessionRunSubmit>,
        ) -> Json<ProfiledSessionRunReceipt> {
            Json(ProfiledSessionRunReceipt {
                session_id,
                run_id: body.run_id,
            })
        }

        let request = serde_json::json!({
            "agent_id": "coding-agent",
            "operation_id": "issue-42:primary",
            "run_id": "flow-run-42-primary",
            "messages": [{
                "id": "flow-message-42",
                "role": "User",
                "content": [{"type": "text", "text": "Implement"}]
            }],
            "execution_requirements": {
                "tool_capability_narrowing": "configured",
                "required_worker_capabilities": ["awaken.flow.coding.v1"]
            }
        });
        let app = profiled_session_run_router(accept);
        for (rule, method, uri, expected) in [
            (
                "I1",
                "POST",
                "/v1/awaken/sessions/session-42/runs",
                StatusCode::OK,
            ),
            (
                "I2",
                "GET",
                "/v1/awaken/sessions/session-42/runs",
                StatusCode::METHOD_NOT_ALLOWED,
            ),
            (
                "I3",
                "POST",
                "/v1/awaken/sessions/session-42/run",
                StatusCode::NOT_FOUND,
            ),
        ] {
            let body = if method == "POST" {
                Body::from(serde_json::to_vec(&request).unwrap())
            } else {
                Body::empty()
            };
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(body)
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), expected, "{rule}");
        }
    }
}
