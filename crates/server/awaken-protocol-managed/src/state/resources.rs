// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: Managed protocol adapter for complete Session Resource commands and public
// @formal reason: receipts. Dynamic path: it validates and lowers external Resource input, then delegates
// @formal reason: replacement and reconciliation to the canonical Session application and persisted
// @formal reason: SessionResourceState; it owns no parallel lifecycle state, compare-and-swap rule, or
// @formal reason: Runtime authorization path. Evidence: resource.files.resource_lifecycle and adjacent
// @formal reason: Managed Resource tests; this is a product-requirement classification, not a direct
// @formal reason: formal-proof claim.
//! Live-session typed input CRUD and durable resource activation coordination.
//! The persisted [`SessionResourceState`](awaken_session_contract::SessionResourceState)
//! is authoritative. Wire DTOs project its desired manifest; Runtime owns
//! realization of that manifest.

use super::*;

impl ManagedState {
    /// Fixture driver for one Resource activation sweep followed by the wire
    /// refresh of every settled Session. Product composition runs the same
    /// sweep in the Session application's lifecycle supervisor, whose committed
    /// facts reach this adapter through lifecycle delivery.
    #[cfg(any(test, feature = "test-support"))]
    pub async fn reconcile_resource_activations(&self) -> usize {
        let report = self
            .application
            .resource_reconciliation()
            .reconcile_resource_activations()
            .await;
        for session in &report.settled {
            if let Err(error) = self.publish_persisted_session(session) {
                tracing::warn!(
                    session = %session.session_id,
                    error = ?error,
                    "Session Resource wire projection refresh remains pending"
                );
            }
        }
        for failure in report.failures {
            tracing::warn!(
                session = %failure.session_id,
                error = %failure.message,
                "Session resource reconciliation remains pending"
            );
        }
        report.settled.len()
    }

    pub(super) fn map_preparation_error(
        error: awaken_session_application::SessionPreparationError,
    ) -> StateError {
        match error {
            awaken_session_application::SessionPreparationError::NotFound => StateError::NotFound,
            awaken_session_application::SessionPreparationError::Conflict => StateError::Conflict,
            awaken_session_application::SessionPreparationError::Rejected(error) => {
                StateError::Run(error)
            }
            awaken_session_application::SessionPreparationError::Unavailable(message) => {
                StateError::Run(RunError::unavailable(message))
            }
        }
    }

    pub async fn replace_resource_manifest(
        &self,
        id: &str,
        body: crate::types::resource::ResourceManifestReplaceParams,
        idempotency_key: Option<String>,
        expected_resource_revision: Option<u64>,
        request_fingerprint: String,
    ) -> Result<crate::types::resource::SessionResourceManifest, StateError> {
        let request = awaken_session_application::SessionResourceManifestRequest {
            inputs: body
                .resources
                .iter()
                .map(crate::types::resource::ResourceInput::to_input_request)
                .collect(),
            expected_resource_revision,
            idempotency_key,
            request_fingerprint,
        };
        let outcome = self.publish_resource_outcome(
            Box::pin(self.roles.resources.replace_session_resource_manifest(
                &awaken_session_types::SessionId::from(id),
                request,
            ))
            .await,
        )?;
        Ok(Self::resource_manifest_view(&outcome.session))
    }

    fn resource_manifest_view(
        session: &awaken_session_contract::PersistedSession,
    ) -> crate::types::resource::SessionResourceManifest {
        let applying = session.resources.pending.is_some();
        crate::types::resource::SessionResourceManifest {
            desired_revision: session.resources.revision,
            applied_revision: session.resources.active_revision(),
            phase: if applying {
                crate::types::resource::ResourceManifestPhase::Applying
            } else {
                crate::types::resource::ResourceManifestPhase::Active
            },
            resources: session
                .resources
                .desired()
                .inputs()
                .iter()
                .map(|input| resolved_resource_dto(session.session_id.as_str(), input))
                .collect(),
        }
    }

    fn map_resource_manifest_error(
        error: awaken_session_application::SessionResourceManifestError,
    ) -> StateError {
        use awaken_session_application::SessionResourceManifestError as Error;
        match error {
            Error::NotFound => StateError::NotFound,
            Error::Terminal | Error::Conflict => StateError::Conflict,
            Error::IdempotencyMismatch => StateError::IdempotencyMismatch,
            Error::Rejected(error) => StateError::Run(error),
            Error::ProjectionAfterCommit { source, .. } => Self::map_preparation_error(source),
            Error::Unavailable(message) => StateError::Run(RunError::unavailable(message)),
        }
    }

    /// Refresh the wire projection from every committed command outcome,
    /// including one whose realization failed after desired truth committed,
    /// so a retry observes the accepted intent instead of inventing another.
    fn publish_resource_outcome(
        &self,
        result: Result<
            awaken_session_application::SessionResourceManifestOutcome,
            awaken_session_application::SessionResourceManifestError,
        >,
    ) -> Result<awaken_session_application::SessionResourceManifestOutcome, StateError> {
        match result {
            Ok(outcome) => {
                self.publish_persisted_session(&outcome.session)?;
                Ok(outcome)
            }
            Err(
                awaken_session_application::SessionResourceManifestError::ProjectionAfterCommit {
                    outcome,
                    source,
                },
            ) => {
                self.publish_persisted_session(&outcome.session)?;
                Err(Self::map_preparation_error(source))
            }
            Err(error) => Err(Self::map_resource_manifest_error(error)),
        }
    }

    fn resource_dto_for_binding(
        session_id: &str,
        state: &awaken_session_contract::SessionResourceState,
        binding_id: &str,
    ) -> Option<crate::types::resource::SessionResource> {
        state
            .desired()
            .inputs()
            .iter()
            .find(|input| input.binding_id.as_str() == binding_id)
            .map(|input| resolved_resource_dto(session_id, input))
    }

    /// Resource reads consume desired Session truth only. They do not need a
    /// Run snapshot, transcript ordering, or a warm protocol cache.
    pub async fn list_resources(
        &self,
        id: &str,
    ) -> Result<(u64, Vec<crate::types::resource::SessionResource>), StateError> {
        let observation = observe_resource_manifest(self, id).await?;
        Ok((observation.revision, observation.manifest.resources))
    }

    pub async fn create_resource(
        &self,
        id: &str,
        body: crate::types::resource::ResourceAddParams,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let request = body.into_resource_input().to_input_request();
        let session_id = awaken_session_types::SessionId::from(id);
        let attachment = self
            .roles
            .resources
            .attach_session_file(&session_id, request)
            .await;
        let (outcome, binding_id) = match attachment {
            Ok(attachment) => (Ok(attachment.outcome), Some(attachment.binding_id)),
            Err(error) => (Err(error), None),
        };
        let outcome = self.publish_resource_outcome(outcome)?;
        // The accepted command already owns the exact durable result. A query
        // optimization must not add a fallible post-effect read or report a
        // concurrent later generation as this command's receipt.
        binding_id
            .and_then(|binding_id| {
                Self::resource_dto_for_binding(id, &outcome.session.resources, binding_id.as_str())
            })
            .ok_or(StateError::NotFound)
    }

    pub async fn get_resource(
        &self,
        id: &str,
        resource_id: &str,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let resource_state = self
            .read_persisted_session_projection(id)
            .await?
            .session
            .resources;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        Self::resource_dto_for_binding(id, &resource_state, binding_id.as_str())
            .ok_or(StateError::NotFound)
    }

    pub async fn update_resource(
        &self,
        id: &str,
        resource_id: &str,
        patch: crate::types::resource::ResourceUpdateParams,
    ) -> Result<crate::types::resource::SessionResource, StateError> {
        let owner_scope = self.resolve_owner(id).await?.ok_or(StateError::NotFound)?;
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        let persisted = self
            .roles
            .credentials
            .rotate_repository_credential(
                &awaken_session_types::SessionId::from(id),
                &owner_scope,
                &binding_id,
                patch.authorization_token.into_credential_material(),
            )
            .await
            .map_err(Self::map_preparation_error)?;
        self.publish_persisted_session(&persisted)?;
        let input = persisted
            .resources
            .desired()
            .inputs()
            .iter()
            .find(|input| input.binding_id == binding_id)
            .ok_or(StateError::NotFound)?;
        Ok(resolved_resource_dto(id, input))
    }

    pub async fn delete_resource(&self, id: &str, resource_id: &str) -> Result<(), StateError> {
        let binding_id = resource_binding_id(id, resource_id).ok_or(StateError::NotFound)?;
        self.publish_resource_outcome(
            self.roles
                .resources
                .detach_session_input(&awaken_session_types::SessionId::from(id), &binding_id)
                .await,
        )?;
        Ok(())
    }
}

/// One read projection shared by the official list and Awaken observation.
pub(crate) async fn observe_resource_manifest(
    state: &ManagedState,
    id: &str,
) -> Result<crate::types::resource::SessionResourceObservation, StateError> {
    let session = state.read_persisted_session_projection(id).await?.session;
    let policy = session.baseline.resource_mutation_policy();
    let mut manifest = ManagedState::resource_manifest_view(&session);
    // The watermark survives rollback; the desired/active pairs do not advance
    // unless their manifests do. The read must not pair old inputs with a new
    // attempted-generation number, nor strand the Console in Preparing.
    manifest.desired_revision = session.resources.desired_generation().0;
    Ok(crate::types::resource::SessionResourceObservation {
        revision: session.resources.revision,
        inputs: session
            .resources
            .desired()
            .inputs()
            .iter()
            .map(input_details)
            .collect(),
        skills: session.resources.desired().skills().to_vec(),
        unapplied_revision: (session.resources.pending.is_none()
            && session.resources.revision > manifest.applied_revision)
            .then_some(session.resources.revision),
        manifest,
        mutations: crate::types::resource::SessionResourceMutations::observe(
            policy,
            session.is_terminal(),
        ),
    })
}

fn input_details(
    input: &crate::session_resource::ResolvedInput,
) -> crate::types::resource::SessionInputDetails {
    use crate::session_resource::ResolvedInputSource;
    let (kind, id, configuration_version) = match &input.source {
        ResolvedInputSource::File { file_id } => ("file", file_id.to_string(), None),
        ResolvedInputSource::MemoryStore {
            memory_store_id,
            config,
        } => (
            "memory_store",
            memory_store_id.to_string(),
            Some(config.version.0.to_string()),
        ),
        ResolvedInputSource::Repository {
            repository_id,
            config,
            ..
        } => (
            "repository",
            repository_id.to_string(),
            Some(config.version.0.to_string()),
        ),
    };
    crate::types::resource::SessionInputDetails {
        binding_id: input.binding_id.to_string(),
        resource_kind: kind,
        target_id: id,
        mount_path: input.mount_path.clone(),
        access: input.access,
        configuration_version,
        instructions: input.instructions.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_session_application::SessionPreparationError;
    use axum::http::StatusCode;

    #[test]
    fn preparation_causes_retain_their_http_classification() {
        // Cause/effect graph: the application owns C1 missing, C2 root CAS
        // conflict, C3 recoverable dependency/cleanup pending, C4 explicit Run
        // rejection. The sole Managed conversion and existing HTTP projector
        // must preserve those causes: E1 404, E2 409, E3 503, E4 the original
        // rejection class. No error yields a successful cleanup receipt.
        // | Rule | cause | effect |
        // | P1 | NotFound | 404/not_found_error |
        // | P2 | Conflict | 409/invalid_request_error |
        // | P3 | Unavailable (cleanup or dependency) | 503/api_error |
        // | P4 | Rejected bad request | 400/invalid_request_error |
        // | P5 | Rejected internal | 500/api_error |
        // | P6 | Rejected unavailable | 503/api_error |
        // HTTP classification grants no retry/replay or physical-stop proof;
        // the same Session operation must still supply verified completion.
        for (rule, cause, expected_status, expected_type) in [
            (
                "P1",
                SessionPreparationError::NotFound,
                StatusCode::NOT_FOUND,
                "not_found_error",
            ),
            (
                "P2",
                SessionPreparationError::Conflict,
                StatusCode::CONFLICT,
                "invalid_request_error",
            ),
            (
                "P3-cleanup",
                SessionPreparationError::Unavailable(
                    "Session archive cleanup remains pending".into(),
                ),
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
            ),
            (
                "P3-dependency",
                SessionPreparationError::Unavailable("dependency unavailable".into()),
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
            ),
            (
                "P4",
                SessionPreparationError::Rejected(RunError::bad_request("invalid input")),
                StatusCode::BAD_REQUEST,
                "invalid_request_error",
            ),
            (
                "P5",
                SessionPreparationError::Rejected(RunError::internal("internal failure")),
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
            ),
            (
                "P6",
                SessionPreparationError::Rejected(RunError::unavailable("temporarily unavailable")),
                StatusCode::SERVICE_UNAVAILABLE,
                "api_error",
            ),
        ] {
            let (status, body) =
                crate::routes::sessions::error_response(ManagedState::map_preparation_error(cause));
            assert_eq!(status, expected_status, "{rule}");
            let wire = serde_json::to_value(body.0).expect("canonical error envelope");
            assert_eq!(wire["error"]["type"], expected_type, "{rule}");
        }
    }
}
