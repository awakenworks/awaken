// @formal surface requirement=resource.files.resource_lifecycle
// @formal reason: Static owner: Session application lowering of authored Resource inputs. Dynamic path:
// @formal reason: it validates, normalizes and resolves inputs and configures/compensates Repository
// @formal reason: participants before the canonical manifest command; it owns no lifecycle state, CAS rule
// @formal reason: or Runtime authorization. This is a product-requirement classification, not a proof claim.
//! Lowering of authored Session inputs into resolved Resource inputs for the
//! caller-issued Resource commands in `resource_reconciliation::manifest_command`.
//!
//! Every identity, reuse, admission, layout, Repository-participant and
//! compensation decision a protocol once made over its wire request is made
//! here, before the one root-CAS manifest command.
//!
//! Persisted identities keep their original spellings: manifest bindings are
//! `session:{id}:manifest:{fingerprint}`, live File bindings
//! `session:{id}:live:{n}`, and manifest Repository participants
//! `managed:{id}:repository:{fingerprint}`.

use std::collections::BTreeMap;

use awaken_agent_contract::stable_fingerprint;
use awaken_resource_contract::{InputBinding, RepositoryId};
use awaken_session_state_contract::PersistedSession;
use awaken_session_types::RunError;

use crate::session_input::{
    InputProjectionClass, admit_file_count, input_binding, preflight_repository_mount_paths,
};
use crate::session_resource::{
    ResolvedInput, ResolvedSessionResources, SessionInputAttachment, SessionInputResolver,
};
use crate::{
    ConfiguredSessionRepository, SessionInputRequest, SessionInputTarget,
    SessionRepositoryResourceInput,
};

pub(crate) fn normalized_mount(path: &str) -> &str {
    path.trim_start_matches('/')
}

/// The persisted binding one authored manifest row occupies. An input already
/// at the same mount and projection class keeps its binding; otherwise the id
/// derives from the mount, qualified by the projection class only when
/// several authored rows share that mount.
fn manifest_binding_id(
    session_id: &str,
    request: &SessionInputRequest,
    same_mount: Option<&ResolvedInput>,
    authored_mount_rows: usize,
) -> String {
    if let Some(existing) = same_mount {
        return existing.binding_id.to_string();
    }
    let mount = normalized_mount(&request.mount_path);
    let fingerprint = if authored_mount_rows > 1 {
        stable_fingerprint(&(mount, InputProjectionClass::of_request(&request.target)))
    } else {
        stable_fingerprint(&mount)
    };
    format!("session:{session_id}:manifest:{fingerprint}")
}

/// The first free live File binding at or after the current input count.
pub(crate) fn live_file_binding_id(session_id: &str, current: &ResolvedSessionResources) -> String {
    (current.inputs().len()..)
        .map(|suffix| format!("session:{session_id}:live:{suffix}"))
        .find(|candidate| {
            current
                .inputs()
                .iter()
                .all(|input| input.binding_id.as_str() != candidate)
        })
        .expect("an unbounded suffix range always has a free binding")
}

/// Resolve Agent defaults and Session attachments against the selected Resource
/// Registry. Creation and every Resource command share this one resolver and
/// its intrinsic error mapping.
pub(crate) async fn resolve_session_inputs(
    registry: Option<&dyn awaken_resource_contract::ResourceRegistry>,
    workspace_id: &str,
    agent_defaults: &[InputBinding],
    session_inputs: &[SessionInputAttachment],
) -> Result<ResolvedSessionResources, RunError> {
    SessionInputResolver::resolve_inputs(
        workspace_id,
        registry.map(|catalog| catalog as &dyn awaken_resource_contract::ExecutionResourceResolver),
        agent_defaults,
        session_inputs,
    )
    .await
    .map_err(crate::runtime_commands::session_input_error)
}

/// One request's lowering of authored inputs into resolved Session inputs.
///
/// It owns the request's Repository participant prefix: every participant it
/// configures is recorded before resolution, and [`Self::compensate`] retires
/// exactly that prefix when the request fails before the root accepts it.
pub(crate) struct ResourceLowering<'a> {
    registry: Option<&'a dyn awaken_resource_contract::ResourceRegistry>,
    participants: &'a crate::SessionRepositoryParticipants,
    placement: &'a awaken_session_execution_contract::SessionPlacementRoles,
    session_id: &'a str,
    owner_scope: &'a str,
    configured: Vec<ConfiguredSessionRepository>,
}

impl<'a> ResourceLowering<'a> {
    pub(crate) fn new(
        registry: Option<&'a dyn awaken_resource_contract::ResourceRegistry>,
        participants: &'a crate::SessionRepositoryParticipants,
        placement: &'a awaken_session_execution_contract::SessionPlacementRoles,
        session_id: &'a str,
        owner_scope: &'a str,
    ) -> Self {
        Self {
            registry,
            participants,
            placement,
            session_id,
            owner_scope,
            configured: Vec::new(),
        }
    }

    /// Derived MemoryStore mounts through the selected Inventory; the same
    /// normalizer Session creation uses.
    pub(crate) async fn normalize(
        &self,
        requests: &[SessionInputRequest],
        agent_defaults: &[InputBinding],
    ) -> Result<Vec<SessionInputRequest>, RunError> {
        crate::session_input::normalize_session_input_mounts(
            self.registry
                .map(|registry| registry as &dyn awaken_resource_contract::ResourceInventory),
            self.owner_scope,
            requests,
            agent_defaults,
        )
        .await
    }

    pub(crate) async fn resolve(
        &self,
        input: SessionInputAttachment,
    ) -> Result<ResolvedInput, RunError> {
        let (mut resolved, _) =
            resolve_session_inputs(self.registry, self.owner_scope, &[], &[input])
                .await?
                .into_parts();
        resolved
            .pop()
            .ok_or_else(|| RunError::internal("resolved resource manifest input is empty"))
    }

    /// Revalidate a proposed complete Resource layout against the Session's
    /// immutable baseline. Provisional bindings suffice: layout does not
    /// depend on catalog versions or credential pins.
    pub(crate) fn validate_layout(
        &self,
        session: &PersistedSession,
        resources: Vec<InputBinding>,
    ) -> Result<(), RunError> {
        let baseline = session
            .frozen_baseline()
            .ok_or_else(|| RunError::unavailable("Session creation intent was not consumed"))?;
        crate::validate_session_sandbox_layout(
            self.placement,
            session.session_id.as_str(),
            &awaken_session_configuration_contract::SessionSandboxLayout {
                workspace_id: self.owner_scope.to_string(),
                agent_id: baseline.agent_id.clone(),
                agent_revision: baseline.agent_revision,
                runtime_placement: baseline.runtime_placement,
                model_override: baseline.model_override.clone(),
                runtime: baseline.runtime.clone(),
                mounts: baseline.mounts.clone(),
                env: baseline.env.clone(),
                environment: baseline.environment.clone(),
                resources,
            },
        )
    }

    pub(crate) async fn compensate(&self, failure_message: &str) {
        self.participants
            .compensate(self.session_id, &self.configured, failure_message)
            .await;
    }

    /// Lower one complete authored manifest. The whole authored set is
    /// admitted and its layout validated before the first participant; a
    /// later failure compensates the configured prefix and returns the first
    /// error.
    pub(crate) async fn manifest(
        &mut self,
        requests: &[SessionInputRequest],
        session: &PersistedSession,
    ) -> Result<ResolvedSessionResources, RunError> {
        // Validate the entire authored Repository set before even a preceding
        // implicit MemoryStore can read its definition. Later phases reuse the
        // same shared rule at the aggregate and provider effect boundaries.
        preflight_repository_mount_paths(requests)?;
        admit_file_count(requests)?;
        let requests = self.normalize(requests, &[]).await?;
        let provisional = requests
            .iter()
            .enumerate()
            .map(|(index, input)| {
                input_binding(
                    format!("manifest-validation-{index}"),
                    input,
                    matches!(input.target, SessionInputTarget::Repository { .. })
                        .then(|| RepositoryId::from("validation")),
                )
            })
            .collect::<Vec<_>>();
        let effective_bindings = SessionInputResolver::effective_bindings(&provisional, &[])
            .map_err(|error| RunError::bad_request(error.to_string()))?;
        self.validate_layout(session, effective_bindings)?;

        let current = session.resources.desired();
        let mut authored_mount_rows = BTreeMap::<&str, usize>::new();
        for input in &requests {
            *authored_mount_rows
                .entry(normalized_mount(&input.mount_path))
                .or_default() += 1;
        }
        let mut inputs = Vec::with_capacity(requests.len());
        for input in &requests {
            let rows = authored_mount_rows
                .get(normalized_mount(&input.mount_path))
                .copied()
                .unwrap_or_default();
            match self.row(current, input, rows).await {
                Ok(resolved) => inputs.push(resolved),
                Err(first) => {
                    self.compensate(
                        "Resource Manifest Repository compensation remains pending after lowering failure",
                    )
                    .await;
                    return Err(first);
                }
            }
        }
        match ResolvedSessionResources::try_new(inputs, current.skills().to_vec()) {
            Ok(resources) => Ok(resources),
            Err(error) => {
                self.compensate(
                    "Resource Manifest Repository compensation remains pending after final validation",
                )
                .await;
                Err(RunError::bad_request(error.to_string()))
            }
        }
    }

    /// Lower one authored row: retain an equal current source, otherwise
    /// configure its Repository participant (if any) and resolve it.
    async fn row(
        &mut self,
        current: &ResolvedSessionResources,
        input: &SessionInputRequest,
        authored_mount_rows: usize,
    ) -> Result<ResolvedInput, RunError> {
        let mount = normalized_mount(&input.mount_path);
        let same_mount = current.inputs().iter().find(|existing| {
            normalized_mount(&existing.mount_path) == mount
                && InputProjectionClass::of_request(&input.target)
                    == InputProjectionClass::of_resolved(&existing.source)
        });
        if let Some(existing) =
            same_mount.filter(|existing| input.target.can_reuse_resolved_source(&existing.source))
        {
            let mut existing = existing.clone();
            existing.access = input.access;
            existing.instructions = input.instructions.clone();
            return Ok(existing);
        }
        let binding_id =
            manifest_binding_id(self.session_id, input, same_mount, authored_mount_rows);
        let repository_id = if let SessionInputTarget::Repository {
            remote_url,
            credential_material,
            initial_branch,
            initial_commit,
        } = &input.target
        {
            let participant = self
                .participants
                .configure(SessionRepositoryResourceInput {
                    id: format!(
                        "managed:{}:repository:{}",
                        self.session_id,
                        stable_fingerprint(&(mount, remote_url, initial_branch, initial_commit))
                    ),
                    workspace_id: self.owner_scope.to_string(),
                    name: format!("Session repository at /{mount}"),
                    description: "Managed Session resource manifest input".into(),
                    remote_url: remote_url.clone(),
                    credential_material: credential_material.clone(),
                    credential: None,
                    mount_path: input.mount_path.clone(),
                    initial_branch: initial_branch.clone(),
                    initial_commit: initial_commit.clone(),
                })
                .await?;
            let repository_id = participant.repository_id.clone();
            self.configured.push(participant);
            Some(repository_id)
        } else {
            None
        };
        self.resolve(SessionInputAttachment {
            binding: input_binding(binding_id, input, repository_id),
            replaces: None,
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_resource::ResolvedInputSource;
    use awaken_resource_contract::{BindingId, FileId, ResourceAccess};

    fn file_input(binding: &str, mount: &str) -> ResolvedInput {
        ResolvedInput {
            created_at_unix_ms: None,
            updated_at_unix_ms: None,
            binding_id: BindingId::new(binding),
            source: ResolvedInputSource::File {
                file_id: FileId::from("f"),
            },
            mount_path: mount.into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    fn request(target: SessionInputTarget, mount: &str) -> SessionInputRequest {
        SessionInputRequest {
            target,
            mount_path: mount.into(),
            access: ResourceAccess::ReadOnly,
            instructions: None,
            derived_memory_mount: false,
        }
    }

    #[test]
    fn manifest_binding_identity_follows_mount_occupancy_and_row_count() {
        // Cause/effect graph: C1 an input of the same projection class already
        // occupies the mount; C2 several authored rows share the mount.
        // E1 keep the occupant's binding; E2 derive from the mount alone; E3
        // derive from the mount and the projection class.
        // | Rule | C1 | C2 | Effect |
        // |---|---|---|---|
        // | B1 | yes | any | E1 |
        // | B2 | no | no | E2 |
        // | B3 | no | yes | E3 |
        let file = request(SessionInputTarget::File(FileId::from("f")), "/workspace/a");
        let b2 = manifest_binding_id("s", &file, None, 1);
        assert_eq!(
            b2,
            format!("session:s:manifest:{}", stable_fingerprint(&"workspace/a")),
            "B2"
        );
        let b3 = manifest_binding_id("s", &file, None, 2);
        assert_eq!(
            b3,
            format!(
                "session:s:manifest:{}",
                stable_fingerprint(&(
                    "workspace/a",
                    InputProjectionClass::of_request(&file.target)
                ))
            ),
            "B3"
        );
        assert_ne!(b2, b3, "B2/B3 distinct");
        let occupant = file_input("session:s:live:0", "/workspace/a");
        for rows in [1, 2] {
            assert_eq!(
                manifest_binding_id("s", &file, Some(&occupant), rows),
                "session:s:live:0",
                "B1 rows={rows}"
            );
        }
    }

    #[test]
    fn live_file_binding_takes_the_first_free_suffix_from_the_input_count() {
        // L1 no input holds `live:{len}` -> that suffix; L2 it is held -> the
        // next free suffix. Empty manifests start at 0.
        let empty = ResolvedSessionResources::default();
        assert_eq!(
            live_file_binding_id("s", &empty),
            "session:s:live:0",
            "L1 empty"
        );
        let one = ResolvedSessionResources::try_new(
            vec![file_input("session:s:live:1", "/workspace/a")],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(
            live_file_binding_id("s", &one),
            "session:s:live:2",
            "L2 held"
        );
    }
}
