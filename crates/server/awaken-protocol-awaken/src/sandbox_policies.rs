// @formal surface requirement=runtime.capabilities.capability_floor
// @formal reason: This protocol adapter publishes and binds exact Sandbox policy revisions under the
// @formal reason: existing capability-floor product requirement; this classification is not a new
// @formal reason: authorization path or a formal-proof claim.
//! Awaken sandbox execution-policy authoring protocol.

use std::collections::BTreeMap;
use std::sync::Arc;

use awaken_environment_application::{EnvironmentApplication, EnvironmentApplicationError};
use awaken_provisioning_contract::{
    SandboxExecutionPolicy, SandboxExecutionPolicyError, SandboxExecutionPolicyId,
    SandboxExecutionPolicyRef, SandboxExecutionPolicyStore, SandboxExecutionPolicyVersion,
};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

struct EnvironmentExtensionsState {
    application: Arc<EnvironmentApplication>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
}

pub fn environment_extensions_router(
    application: Arc<EnvironmentApplication>,
    sandbox_policies: Option<Arc<dyn SandboxExecutionPolicyStore>>,
) -> Router {
    Router::new()
        .route("/v1/awaken/environments", post(create_environment))
        .route(
            "/v1/awaken/sandbox-execution-policies",
            post(create_sandbox_policy),
        )
        .route(
            "/v1/awaken/sandbox-execution-policies/{id}/versions",
            post(publish_sandbox_policy),
        )
        .route(
            "/v1/awaken/sandbox-execution-policies/{id}/versions/{version}",
            get(get_exact_sandbox_policy),
        )
        .route(
            "/v1/awaken/environments/{id}/sandbox-execution-policy",
            get(get_environment_sandbox_policy).post(bind_environment_sandbox_policy),
        )
        .with_state(Arc::new(EnvironmentExtensionsState {
            application,
            sandbox_policies,
        }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyCreate {
    id: String,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_session_configuration_contract::SandboxProvisioning,
    #[serde(default)]
    idle_retention: awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyPublish {
    expected_current: u64,
    config: awaken_provisioning_contract::SandboxOverride,
    #[serde(default)]
    provisioning: awaken_session_configuration_contract::SandboxProvisioning,
    #[serde(default)]
    idle_retention: awaken_session_configuration_contract::EnvironmentIdleRetentionPolicy,
    #[serde(default)]
    disabled: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SandboxPolicyBindingInput {
    policy_id: String,
    version: u64,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentCreateInput {
    command_id: String,
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    metadata: BTreeMap<String, String>,
    #[serde(default)]
    scope: Option<String>,
    config: awaken_environment_contract::EnvironmentConfig,
    #[serde(default)]
    sandbox_policy: Option<SandboxPolicyBindingInput>,
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
struct SandboxPolicyBindingOutput {
    environment_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    policy_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<u64>,
    provisioning: awaken_session_configuration_contract::SandboxProvisioning,
}

fn policy_store(
    state: &EnvironmentExtensionsState,
) -> Result<Arc<dyn SandboxExecutionPolicyStore>, StatusCode> {
    state
        .sandbox_policies
        .clone()
        .ok_or(StatusCode::SERVICE_UNAVAILABLE)
}

async fn project_policy_binding(
    state: &EnvironmentExtensionsState,
    environment_id: String,
    reference: SandboxExecutionPolicyRef,
) -> Result<SandboxPolicyBindingOutput, StatusCode> {
    let policy = policy_store(state)?
        .get_exact(&reference)
        .await
        .map_err(map_policy_error)?;
    Ok(SandboxPolicyBindingOutput {
        environment_id,
        policy_id: Some(reference.id.0),
        version: Some(reference.version.0),
        provisioning: policy.provisioning,
    })
}

async fn create_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Json(input): Json<SandboxPolicyCreate>,
) -> Result<(StatusCode, Json<SandboxExecutionPolicy>), StatusCode> {
    let policy = SandboxExecutionPolicy {
        id: SandboxExecutionPolicyId(input.id),
        version: SandboxExecutionPolicyVersion::INITIAL,
        config: input.config,
        provisioning: input.provisioning,
        idle_retention: input.idle_retention,
        disabled: input.disabled,
    };
    policy_store(&state)?
        .create(policy.clone())
        .await
        .map_err(map_policy_error)?;
    Ok((StatusCode::CREATED, Json(policy)))
}

async fn publish_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path(id): Path<String>,
    Json(input): Json<SandboxPolicyPublish>,
) -> Result<Json<SandboxExecutionPolicy>, StatusCode> {
    let current = SandboxExecutionPolicyVersion(input.expected_current);
    // The value type owns succession; this adapter preserves the existing
    // exhausted-version conflict before checking store availability.
    let next = current.checked_next().map_err(|_| StatusCode::CONFLICT)?;
    let policy = SandboxExecutionPolicy {
        id: SandboxExecutionPolicyId(id),
        version: next,
        config: input.config,
        provisioning: input.provisioning,
        idle_retention: input.idle_retention,
        disabled: input.disabled,
    };
    policy_store(&state)?
        .publish(current, policy.clone())
        .await
        .map_err(map_policy_error)?;
    Ok(Json(policy))
}

async fn get_exact_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path((id, version)): Path<(String, u64)>,
) -> Result<Json<SandboxExecutionPolicy>, StatusCode> {
    let policy = policy_store(&state)?
        .get_exact(&SandboxExecutionPolicyRef {
            id: SandboxExecutionPolicyId(id),
            version: SandboxExecutionPolicyVersion(version),
        })
        .await
        .map_err(map_policy_error)?;
    Ok(Json(policy))
}

async fn create_environment(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Json(input): Json<EnvironmentCreateInput>,
) -> Result<(StatusCode, Json<awaken_environment_contract::EnvItem>), StatusCode> {
    let sandbox_policy = input.sandbox_policy.map(|reference| {
        awaken_environment_contract::EnvironmentSandboxPolicyRef {
            policy_id: reference.policy_id,
            version: reference.version,
        }
    });
    let item = state
        .application
        .create(awaken_environment_contract::CreateEnvironmentCommand {
            command_id: input.command_id,
            name: input.name,
            description: input.description,
            metadata: input.metadata,
            scope: input.scope,
            config: input.config,
            sandbox_policy,
        })
        .await
        .map_err(map_application_error)?;
    Ok((StatusCode::CREATED, Json(item)))
}

async fn bind_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path(environment_id): Path<String>,
    Json(input): Json<SandboxPolicyBindingInput>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let reference = SandboxExecutionPolicyRef {
        id: SandboxExecutionPolicyId(input.policy_id),
        version: SandboxExecutionPolicyVersion(input.version),
    };
    state
        .application
        .bind_sandbox_policy(&environment_id, reference.clone())
        .await
        .map_err(map_application_error)?;
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

async fn get_environment_sandbox_policy(
    State(state): State<Arc<EnvironmentExtensionsState>>,
    Path(environment_id): Path<String>,
) -> Result<Json<SandboxPolicyBindingOutput>, StatusCode> {
    let environment = state
        .application
        .get(&environment_id)
        .await
        .map_err(map_application_error)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let Some(reference) = environment
        .sandbox_policy
        .map(|reference| SandboxExecutionPolicyRef {
            id: SandboxExecutionPolicyId(reference.policy_id),
            version: SandboxExecutionPolicyVersion(reference.version),
        })
    else {
        return Ok(Json(SandboxPolicyBindingOutput {
            environment_id,
            policy_id: None,
            version: None,
            provisioning: Default::default(),
        }));
    };
    Ok(Json(
        project_policy_binding(&state, environment_id, reference).await?,
    ))
}

fn map_application_error(error: EnvironmentApplicationError) -> StatusCode {
    match error {
        EnvironmentApplicationError::NotFound => StatusCode::NOT_FOUND,
        EnvironmentApplicationError::BuiltinImmutable | EnvironmentApplicationError::Archived => {
            StatusCode::CONFLICT
        }
        EnvironmentApplicationError::Policy(error) => map_policy_error(error),
        EnvironmentApplicationError::PolicyStoreUnavailable => StatusCode::SERVICE_UNAVAILABLE,
        EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::IdempotencyConflict
            | awaken_environment_contract::CreateEnvironmentError::OriginalResultUnavailable {
                ..
            },
        ) => StatusCode::CONFLICT,
        EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::InvalidConfig(_),
        )
        | EnvironmentApplicationError::Store(
            awaken_environment_contract::EnvironmentStoreError::InvalidConfig(_),
        ) => StatusCode::UNPROCESSABLE_ENTITY,
        EnvironmentApplicationError::Store(
            awaken_environment_contract::EnvironmentStoreError::RevisionConflict
            | awaken_environment_contract::EnvironmentStoreError::RevisionExhausted,
        ) => StatusCode::CONFLICT,
        EnvironmentApplicationError::Create(
            awaken_environment_contract::CreateEnvironmentError::Store(_),
        )
        | EnvironmentApplicationError::Registration(_)
        | EnvironmentApplicationError::Store(
            awaken_environment_contract::EnvironmentStoreError::Backend(_),
        )
        | EnvironmentApplicationError::RegistrationInvariant(_)
        | EnvironmentApplicationError::CommittedRegistrationPending { .. } => {
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
}

fn map_policy_error(error: SandboxExecutionPolicyError) -> StatusCode {
    match error {
        SandboxExecutionPolicyError::NotFound => StatusCode::NOT_FOUND,
        SandboxExecutionPolicyError::VersionConflict => StatusCode::CONFLICT,
        SandboxExecutionPolicyError::Disabled | SandboxExecutionPolicyError::Invalid(_) => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        SandboxExecutionPolicyError::StoreFailed(_) => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_executable_environment_catalog::{
        ExecutableEnvironmentCatalog, LocalExecutableEnvironmentRegistrar,
    };
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use tower::ServiceExt;

    #[test]
    fn sandbox_policy_error_decision_table_is_explicit() {
        // Cause/effect rules: C1 absent policy -> E1 404; C2 stale publish ->
        // E2 409; C3 invalid/disabled policy -> E3 422; C4 store failure -> E4
        // 500. This is Awaken-only wire behavior, never an Anthropic endpoint.
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::NotFound),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::VersionConflict),
            StatusCode::CONFLICT
        );
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::Disabled),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            map_policy_error(SandboxExecutionPolicyError::StoreFailed("x".into())),
            StatusCode::INTERNAL_SERVER_ERROR
        );
        // Application composition must preserve the same domain cause instead
        // of string-erasing NotFound into the generic 422 arm. C5 an absent
        // application store is distinct infrastructure unavailability -> E5 503.
        assert_eq!(
            map_application_error(EnvironmentApplicationError::Policy(
                SandboxExecutionPolicyError::NotFound
            )),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            map_application_error(EnvironmentApplicationError::PolicyStoreUnavailable),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_application_error(EnvironmentApplicationError::Create(
                awaken_environment_contract::CreateEnvironmentError::IdempotencyConflict,
            )),
            StatusCode::CONFLICT,
            "C5a conflicting command replay is a domain conflict"
        );
        // C5b a domain-invalid Environment is a client error regardless of
        // whether it is rejected on create or update -> E5b 422.
        assert_eq!(
            map_application_error(EnvironmentApplicationError::Create(
                awaken_environment_contract::CreateEnvironmentError::InvalidConfig(
                    awaken_environment_contract::InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" },
                ),
            )),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        assert_eq!(
            map_application_error(EnvironmentApplicationError::Store(
                awaken_environment_contract::EnvironmentStoreError::InvalidConfig(
                    awaken_environment_contract::InvalidEnvironmentConfig::InvalidPackageRequirement { manager: "npm" },
                ),
            )),
            StatusCode::UNPROCESSABLE_ENTITY
        );
        // C5c another author advances the Environment after policy validation
        // -> E5c 409, so the caller retries against the new aggregate revision.
        // A stale or exhausted revision is a domain conflict, never a backend outage.
        for error in [
            awaken_environment_contract::EnvironmentStoreError::RevisionConflict,
            awaken_environment_contract::EnvironmentStoreError::RevisionExhausted,
        ] {
            assert_eq!(
                map_application_error(EnvironmentApplicationError::Store(error)),
                StatusCode::CONFLICT
            );
        }
        // C6 outbox unavailable or structurally inconsistent -> E6 503; callers
        // must not observe authoring success before executable acknowledgement.
        assert_eq!(
            map_application_error(EnvironmentApplicationError::Store(
                awaken_environment_contract::EnvironmentStoreError::Backend("x".into()),
            )),
            StatusCode::SERVICE_UNAVAILABLE
        );
        assert_eq!(
            map_application_error(EnvironmentApplicationError::RegistrationInvariant(
                "x".into()
            )),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[tokio::test]
    async fn environment_create_selects_an_exact_policy_atomically() {
        // Cause/effect graph: C1 Environment kind={self_hosted,cloud}; C2 exact
        // policy={trusted_host,missing}; C3 binding={present,absent}. Effects:
        // E1 a compatible exact version is persisted in revision 1 and returned;
        // E2 missing policy -> 404 and no Environment fact; E3 cloud+trusted ->
        // 422 and no Environment fact. Constraints: trusted_host requires both
        // self_hosted and workdir, and creation cannot expose a create-then-bind
        // intermediate state.
        //
        // | Rule | C1          | C2           | C3      | Effect |
        // | R1   | self_hosted | trusted_host | present | E1     |
        // | R2   | self_hosted | missing      | present | E2     |
        // | R3   | cloud       | trusted_host | present | E3     |
        let store =
            Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
        store
            .create(SandboxExecutionPolicy {
                id: SandboxExecutionPolicyId("trusted-local".into()),
                version: SandboxExecutionPolicyVersion::INITIAL,
                config: awaken_provisioning_contract::SandboxOverride {
                    isolation: Some(awaken_provisioning_contract::IsolationClass::Workdir),
                    host_access: awaken_provisioning_contract::HostAccessPolicy::TrustedHost,
                    ..Default::default()
                },
                provisioning: Default::default(),
                idle_retention: Default::default(),
                disabled: false,
            })
            .await
            .unwrap();
        let envs = Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
            awaken_time::system_clock(),
        ));
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let application = Arc::new(EnvironmentApplication::new(
            envs.clone(),
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
            Some(store.clone()),
        ));
        let app = environment_extensions_router(application, Some(store));
        let create =
            |command_id: &'static str, config: serde_json::Value, policy_id: &'static str| {
                Request::post("/v1/awaken/environments")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&serde_json::json!({
                            "command_id": command_id,
                            "name": command_id,
                            "config": config,
                            "sandbox_policy": {"policy_id": policy_id, "version": 1}
                        }))
                        .unwrap(),
                    ))
                    .unwrap()
            };

        let response = app
            .clone()
            .oneshot(create(
                "R1",
                serde_json::json!({"type": "self_hosted"}),
                "trusted-local",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::CREATED, "R1");
        let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
        let created: awaken_environment_contract::EnvItem = serde_json::from_slice(&body).unwrap();
        assert_eq!(created.revision.0, 1, "R1");
        assert_eq!(
            created.sandbox_policy,
            Some(awaken_environment_contract::EnvironmentSandboxPolicyRef {
                policy_id: "trusted-local".into(),
                version: 1,
            }),
            "R1"
        );

        let missing = app
            .clone()
            .oneshot(create(
                "R2",
                serde_json::json!({"type": "self_hosted"}),
                "missing",
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND, "R2");
        let incompatible = app
            .oneshot(create(
                "R3",
                serde_json::json!({
                    "type": "cloud",
                    "networking": {"type": "unrestricted"},
                    "packages": {}
                }),
                "trusted-local",
            ))
            .await
            .unwrap();
        assert_eq!(
            incompatible.status(),
            StatusCode::UNPROCESSABLE_ENTITY,
            "R3"
        );
        assert_eq!(
            awaken_environment_contract::EnvironmentDefinitionQueries::list_all(envs.as_ref())
                .await
                .unwrap()
                .len(),
            1,
            "R2/R3 must not persist partial Environments"
        );
    }

    #[tokio::test]
    async fn existing_environment_without_binding_projects_the_eager_default() {
        // Cause/effect graph: Environment existence and optional policy binding
        // are independent. An absent Environment is still 404, while an existing
        // Environment without a binding has the effective eager default. The UI
        // can therefore render the ordinary state without manufacturing an HTTP
        // failure or treating absence as an authorization problem.
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let application = Arc::new(EnvironmentApplication::new(
            Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
                awaken_time::system_clock(),
            )),
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
            Some(Arc::new(
                awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default(),
            )),
        ));
        let created = application
            .create(awaken_environment_contract::CreateEnvironmentCommand {
                command_id: "default-binding".into(),
                name: "Default binding".into(),
                description: Some(String::new()),
                metadata: Default::default(),
                scope: None,
                config: awaken_environment_contract::EnvironmentConfig::SelfHosted,
                sandbox_policy: None,
            })
            .await
            .expect("create the unbound Environment fixture");
        let environment_id = created.id;
        let app = environment_extensions_router(application, None);

        let response = app
            .clone()
            .oneshot(
                Request::get(format!(
                    "/v1/awaken/environments/{environment_id}/sandbox-execution-policy"
                ))
                .body(Body::empty())
                .expect("build the policy projection request"),
            )
            .await
            .expect("serve the policy projection request");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read the policy projection response");
        assert_eq!(
            serde_json::from_slice::<SandboxPolicyBindingOutput>(&body)
                .expect("decode the typed policy projection"),
            SandboxPolicyBindingOutput {
                environment_id,
                policy_id: None,
                version: None,
                provisioning: awaken_session_configuration_contract::SandboxProvisioning::Eager,
            }
        );
        let encoded = std::str::from_utf8(&body).expect("projection response is UTF-8 JSON");
        assert!(!encoded.contains("policy_id"));
        assert!(!encoded.contains("version"));

        let missing = app
            .oneshot(
                Request::get("/v1/awaken/environments/missing/sandbox-execution-policy")
                    .body(Body::empty())
                    .expect("build the missing Environment request"),
            )
            .await
            .expect("serve the missing Environment request");
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn exact_policy_http_read_is_versioned_and_fail_closed() {
        // Cause/effect graph: immutable create/publish commands -> exact store
        // versions -> GET(id,version), never the mutable current pointer.
        // Decision table: R0 create/publish HTTP commands carry resource
        // requests into immutable versions; R1 v1 exists -> return the exact v1 config; R2 v2
        // exists with different config -> each URL preserves its own version;
        // R3 absent id/version -> 404; R4 unavailable policy store -> 503. The
        // successor/store decision table adds R5 exhausted+available -> 409
        // without changing either exact version; R6 exhausted+unavailable ->
        // 409 before store admission; R7 nonexhausted+unavailable -> 503.
        // test drives the public Awaken-only router so path extraction, status
        // mapping, and response serialization are covered together.
        let store =
            Arc::new(awaken_sandbox_policy_store::InMemorySandboxExecutionPolicyStore::default());
        let first = SandboxExecutionPolicy {
            id: SandboxExecutionPolicyId("design-runtime".into()),
            version: SandboxExecutionPolicyVersion::INITIAL,
            config: awaken_provisioning_contract::SandboxOverride {
                isolation: Some(awaken_provisioning_contract::IsolationClass::Container),
                requests: awaken_provisioning_contract::ResourceRequests {
                    cpu_millis: Some(500),
                    memory_bytes: Some(1_073_741_824),
                    disk_bytes: Some(2_147_483_648),
                },
                ..Default::default()
            },
            provisioning: Default::default(),
            idle_retention: Default::default(),
            disabled: false,
        };
        let mut second = first.clone();
        second.version = SandboxExecutionPolicyVersion(2);
        second.config.requests.cpu_millis = Some(750);
        let catalog = Arc::new(ExecutableEnvironmentCatalog::new());
        let application = Arc::new(EnvironmentApplication::new(
            Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
                awaken_time::system_clock(),
            )),
            Arc::new(LocalExecutableEnvironmentRegistrar::new(catalog)),
            Some(store.clone()),
        ));
        let app = environment_extensions_router(application, Some(store));
        let get = |path: &'static str| Request::get(path).body(Body::empty()).expect("GET request");
        let post = |path: &'static str, body: serde_json::Value| {
            Request::post(path)
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&body).unwrap()))
                .expect("POST request")
        };

        let created = app
            .clone()
            .oneshot(post(
                "/v1/awaken/sandbox-execution-policies",
                serde_json::json!({
                    "id": "design-runtime",
                    "config": {
                        "isolation": "container",
                        "requests": {
                            "cpu_millis": 500,
                            "memory_bytes": 1_073_741_824u64,
                            "disk_bytes": 2_147_483_648u64
                        }
                    }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(created.status(), StatusCode::CREATED, "R0");
        let published = app
            .clone()
            .oneshot(post(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions",
                serde_json::json!({
                    "expected_current": 1,
                    "config": {
                        "isolation": "container",
                        "requests": {
                            "cpu_millis": 750,
                            "memory_bytes": 1_073_741_824u64,
                            "disk_bytes": 2_147_483_648u64
                        }
                    }
                }),
            ))
            .await
            .unwrap();
        assert_eq!(published.status(), StatusCode::OK, "R0");

        let exhausted = app
            .clone()
            .oneshot(post(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions",
                serde_json::json!({"expected_current": u64::MAX, "config": {}}),
            ))
            .await
            .unwrap();
        assert_eq!(exhausted.status(), StatusCode::CONFLICT, "R5");

        let response = app
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/1",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "R1");
        let exact: SandboxExecutionPolicy =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(exact, first, "R1");

        let response = app
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/2",
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "R2");
        let exact: SandboxExecutionPolicy =
            serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap())
                .unwrap();
        assert_eq!(exact, second, "R2");

        let missing = app
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/3",
            ))
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND, "R3");

        let unavailable_router = environment_extensions_router(
            Arc::new(EnvironmentApplication::new(
                Arc::new(awaken_env_store::InMemoryEnvRegistry::new(
                    awaken_time::system_clock(),
                )),
                Arc::new(LocalExecutableEnvironmentRegistrar::new(Arc::new(
                    ExecutableEnvironmentCatalog::new(),
                ))),
                None,
            )),
            None,
        );
        let unavailable = unavailable_router
            .clone()
            .oneshot(get(
                "/v1/awaken/sandbox-execution-policies/design-runtime/versions/1",
            ))
            .await
            .unwrap();
        assert_eq!(unavailable.status(), StatusCode::SERVICE_UNAVAILABLE, "R4");
        for (expected_current, status, rule) in [
            (u64::MAX, StatusCode::CONFLICT, "R6"),
            (1, StatusCode::SERVICE_UNAVAILABLE, "R7"),
        ] {
            let response = unavailable_router
                .clone()
                .oneshot(post(
                    "/v1/awaken/sandbox-execution-policies/design-runtime/versions",
                    serde_json::json!({"expected_current": expected_current, "config": {}}),
                ))
                .await
                .unwrap();
            assert_eq!(response.status(), status, "{rule}");
        }
    }
}
