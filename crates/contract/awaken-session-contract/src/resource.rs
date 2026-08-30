//! The neutral session-mounted resource (ADR-0038). The Managed wire adapter owns
//! the `resources[]` parse form + the DTO projection; this is the crate-boundary
//! shape both sides speak.

use std::collections::HashSet;

use awaken_resource_contract::{
    BindingId, ExecutionResourceResolver, InputBinding, InputResourceId, MemoryStoreConfigVersion,
    RepositoryConfigVersion, ResourceRegistryError,
};
use serde::{Deserialize, Deserializer, Serialize, de};

/// Pure Session input resolver. Runtime receives only this resolved output
/// and never reads the Agent binding repository itself.
pub struct SessionInputResolver;

/// A temporary Session input and, when present, the exact Agent binding it
/// replaces. Replacement is explicit; array order is never authority.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionInputAttachment {
    pub binding: InputBinding,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<BindingId>,
}

/// Exact, secret-free credential decision frozen for one Repository input.
/// Resource Registry configuration keeps only its Vault binding; the Session
/// application resolves that binding once into this execution pin before any
/// Runtime or Git side effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedRepositoryCredential {
    pub access: awaken_credential_contract::CredentialAccess,
    pub selected_plaintext_holder: awaken_credential_contract::PlaintextHolder,
}

/// Canonical consumption contract for a host-mediated HTTPS Git operation. The
/// Worker adapter performs the Basic transformation from typed username/password
/// material; no preformatted header or token convention crosses this boundary.
#[must_use]
pub fn repository_transport_credential_usage() -> awaken_credential_contract::CredentialUsage {
    awaken_credential_contract::CredentialUsage::HttpBasicAuth
}

/// Derive the one exact material binding shared by Session credential pinning
/// and the deployment-selected Repository transport. Runtime- and
/// transport-local coordinates are deliberately absent: the durable authority
/// is the owning Workspace plus the frozen Repository identity and config
/// version.
#[must_use]
pub fn repository_transport_material_binding(
    workspace_id: &str,
    repository_id: &awaken_resource_contract::RepositoryId,
    config_version: awaken_resource_contract::ConfigVersion,
) -> awaken_credential_contract::CredentialMaterialBinding {
    let usage = repository_transport_credential_usage();
    awaken_credential_contract::CredentialMaterialBinding::for_target(
        workspace_id,
        &(repository_id, config_version),
        &usage,
    )
}

/// Derive the one provider-neutral credential target for an HTTPS Repository
/// remote. Session compilation, retained-pin validation, and credential ingress
/// all call this owner instead of independently interpreting the URL.
pub fn repository_transport_credential_target(
    remote_url: &str,
) -> Result<
    awaken_credential_contract::CredentialTarget,
    awaken_credential_contract::CredentialDescriptorError,
> {
    Ok(awaken_credential_contract::CredentialTarget::new(
        awaken_credential_contract::CredentialPurpose::RepositoryTransport,
        awaken_credential_contract::repository_transport_audience(remote_url)?,
    ))
}

impl ResolvedRepositoryCredential {
    /// Validate the cross-context pin without opening material. This is consumed
    /// both before persistence and at Runtime activation, so malformed retained
    /// rows fail closed through the same rule.
    pub fn validate_for_repository(
        &self,
        binding: &str,
        remote_url: &str,
    ) -> Result<(), SessionInputError> {
        if self.access.credential.id != binding {
            return Err(SessionInputError::InvalidCredentialPin(
                "Repository credential pin selects another source".into(),
            ));
        }
        let expected_target = repository_transport_credential_target(remote_url)
            .map_err(|error| SessionInputError::InvalidCredentialPin(error.to_string()))?;
        if self.access.target.as_ref() != Some(&expected_target) {
            return Err(SessionInputError::InvalidCredentialPin(
                "Repository credential pin targets another HTTPS origin".into(),
            ));
        }
        if self.access.usage != repository_transport_credential_usage() {
            return Err(SessionInputError::InvalidCredentialPin(
                "Repository credential pin has incompatible transport usage".into(),
            ));
        }
        if !self
            .access
            .policy
            .allowed_plaintext_holders
            .contains(&self.selected_plaintext_holder)
        {
            return Err(SessionInputError::InvalidCredentialPin(
                "Repository credential holder is not authorized".into(),
            ));
        }
        if self.access.policy.model_exposure
            != awaken_credential_contract::ModelExposurePolicy::Forbidden
        {
            return Err(SessionInputError::InvalidCredentialPin(
                "Repository credential must remain model-invisible".into(),
            ));
        }
        Ok(())
    }
}

/// Resource-specific, secret-free configuration frozen once for a Session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ResolvedInputSource {
    File {
        file_id: awaken_resource_contract::FileId,
    },
    MemoryStore {
        memory_store_id: awaken_resource_contract::MemoryStoreId,
        config: MemoryStoreConfigVersion,
    },
    Repository {
        repository_id: awaken_resource_contract::RepositoryId,
        config: RepositoryConfigVersion,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential: Option<Box<ResolvedRepositoryCredential>>,
    },
}

/// One entry in the effective manifest handed to activation. Mutable Memory
/// content versions and Git revisions are deliberately absent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedInput {
    pub binding_id: BindingId,
    pub source: ResolvedInputSource,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Exact immutable Skill version selected for a Session. Bundle bytes remain in
/// the resource repository; this durable pin is secret-free and retry-safe.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedSkillBinding {
    #[serde(default)]
    pub kind: awaken_agent_contract::AgentSkillKind,
    pub skill_id: String,
    pub version: u64,
    pub bundle_sha256: String,
}

/// Provider-neutral presence facts shared by Worker admission and Runtime
/// placement. The Session resource aggregate remains the source of retained
/// inputs; this value only prevents adapters from independently classifying the
/// same File/Memory/Repository/Skill set.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SessionResourceCompatibilityFacts {
    has_resources: bool,
    has_repository: bool,
    has_readonly_input: bool,
    has_credentialed_repository: bool,
}

impl SessionResourceCompatibilityFacts {
    #[must_use]
    pub const fn has_resources(self) -> bool {
        self.has_resources
    }

    #[must_use]
    pub const fn has_repository(self) -> bool {
        self.has_repository
    }

    #[must_use]
    pub const fn has_readonly_input(self) -> bool {
        self.has_readonly_input
    }

    #[must_use]
    pub const fn has_credentialed_repository(self) -> bool {
        self.has_credentialed_repository
    }
}

pub(crate) fn session_resource_compatibility_facts(
    inputs: &[ResolvedInput],
    skills: &[ResolvedSkillBinding],
) -> SessionResourceCompatibilityFacts {
    let mut facts = SessionResourceCompatibilityFacts {
        has_resources: !skills.is_empty(),
        ..SessionResourceCompatibilityFacts::default()
    };
    for input in inputs {
        facts.has_resources = true;
        facts.has_readonly_input |=
            input.access == awaken_resource_contract::ResourceAccess::ReadOnly;
        if let ResolvedInputSource::Repository { config, .. } = &input.source {
            facts.has_repository = true;
            facts.has_credentialed_repository |= config.credential_binding.is_some();
        }
    }
    facts
}

/// Durable, secret-free result of the Session control plane's one resolution.
/// Inputs and Skill capabilities remain distinct collections because Skills are
/// executable capabilities, not mounted user inputs. An empty list is the one
/// canonical representation of a Session that selected no Skills.
///
/// ```compile_fail
/// use awaken_session_contract::ResolvedSessionResources;
///
/// let _ = ResolvedSessionResources {
///     inputs: Vec::new(),
///     skills: Vec::new(),
/// };
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ResolvedSessionResources {
    inputs: Vec<ResolvedInput>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    skills: Vec<ResolvedSkillBinding>,
}

#[derive(Deserialize)]
struct ResolvedSessionResourcesWire {
    inputs: Vec<ResolvedInput>,
    #[serde(default)]
    skills: Vec<ResolvedSkillBinding>,
}

impl<'de> Deserialize<'de> for ResolvedSessionResources {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = ResolvedSessionResourcesWire::deserialize(deserializer)?;
        Self::try_from_durable_wire(wire.inputs, wire.skills).map_err(de::Error::custom)
    }
}

/// Frozen, secret-free resource input installed before a Session execution
/// environment is opened on any worker.
///
/// [`ResolvedSessionResources`] deliberately contains no tenant coordinate: it is
/// the value owned by the Session aggregate. Durable dispatch adds the trusted
/// Workspace partition beside it in this envelope so a cold worker can validate
/// and realize the exact same inputs without consulting current Agent defaults.
/// Authorization identities and decisions remain outside this value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionResourceManifest {
    pub workspace_id: String,
    /// Durable selected Session resource generation, paired with `resources`.
    /// Zero is the legacy/create-time generation. A live manifest carries the
    /// selected active or pending generation; after rollback this can be lower
    /// than `SessionResourceState::revision`, the attempted-generation watermark.
    #[serde(default)]
    pub revision: u64,
    pub resources: ResolvedSessionResources,
}

impl SessionResourceManifest {
    #[must_use]
    pub fn new(workspace_id: impl Into<String>, resources: ResolvedSessionResources) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            revision: 0,
            resources,
        }
    }

    #[must_use]
    pub fn at_revision(
        workspace_id: impl Into<String>,
        revision: u64,
        resources: ResolvedSessionResources,
    ) -> Self {
        Self {
            workspace_id: workspace_id.into(),
            revision,
            resources,
        }
    }
}

/// One authoritative physical Resource transition.
///
/// The Session aggregate supplies both generations atomically. Runtime adapters
/// must never reconstruct `previous` from a process-local manifest: that cache
/// can be absent or can already contain `desired` after a crash between a
/// durable Environment-path reservation and the corresponding File/Git effect.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionResourceTransition {
    previous: SessionResourceManifest,
    desired: SessionResourceManifest,
}

#[derive(Deserialize)]
struct SessionResourceTransitionWire {
    previous: SessionResourceManifest,
    desired: SessionResourceManifest,
}

impl<'de> Deserialize<'de> for SessionResourceTransition {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let wire = SessionResourceTransitionWire::deserialize(deserializer)?;
        Self::new(wire.previous, wire.desired).map_err(de::Error::custom)
    }
}

impl SessionResourceTransition {
    pub fn new(
        previous: SessionResourceManifest,
        desired: SessionResourceManifest,
    ) -> Result<Self, SessionInputError> {
        if previous.workspace_id != desired.workspace_id {
            return Err(SessionInputError::InvalidTransition(
                "Resource transition generations belong to different Workspaces".into(),
            ));
        }
        Ok(Self { previous, desired })
    }

    /// Stable identity of the complete forward or rollback operation. Both
    /// generations participate, so A→B, its replay, and B→A cannot alias a
    /// reservation receipt merely because one endpoint revision is reused.
    #[must_use]
    pub fn operation_fingerprint(&self) -> String {
        crate::stable_fingerprint(&(
            "session-resource-transition-v1",
            &self.previous,
            &self.desired,
        ))
    }

    #[must_use]
    pub fn reversed(&self) -> Self {
        Self {
            previous: self.desired.clone(),
            desired: self.previous.clone(),
        }
    }

    #[must_use]
    pub fn previous(&self) -> &SessionResourceManifest {
        &self.previous
    }

    #[must_use]
    pub fn desired(&self) -> &SessionResourceManifest {
        &self.desired
    }
}

impl ResolvedSessionResources {
    /// Construct one complete effective manifest. File and Memory paths
    /// normalize here; Repository paths are validated from their authored raw
    /// bytes and are never rewritten. Every whole-set identity and Repository
    /// tree invariant is checked before the value can cross an application or
    /// persistence boundary.
    pub fn try_new(
        mut inputs: Vec<ResolvedInput>,
        skills: Vec<ResolvedSkillBinding>,
    ) -> Result<Self, SessionInputError> {
        validate_resolved_inputs(&mut inputs, RepositoryPathPolicy::Current)?;
        validate_resolved_skills(&skills)?;
        Ok(Self { inputs, skills })
    }

    /// Decode the one historical Repository location that predates the
    /// `/workspace` contract. This is deliberately private and reachable only
    /// from durable serde replay: new commands and public Rust construction use
    /// [`Self::try_new`] and therefore cannot admit it. The bytes remain exact
    /// so replay never changes a persisted fingerprint; Runtime admission still
    /// rejects the legacy layout before any provider or resource effect.
    fn try_from_durable_wire(
        mut inputs: Vec<ResolvedInput>,
        skills: Vec<ResolvedSkillBinding>,
    ) -> Result<Self, SessionInputError> {
        validate_resolved_inputs(&mut inputs, RepositoryPathPolicy::DurableReplay)?;
        validate_resolved_skills(&skills)?;
        Ok(Self { inputs, skills })
    }

    #[must_use]
    pub fn inputs(&self) -> &[ResolvedInput] {
        &self.inputs
    }

    #[must_use]
    pub fn skills(&self) -> &[ResolvedSkillBinding] {
        &self.skills
    }

    /// Classify this complete generation through the one neutral Resource fact
    /// projector shared by Run placement and terminal-cleanup admission.
    #[must_use]
    pub fn compatibility_facts(&self) -> SessionResourceCompatibilityFacts {
        session_resource_compatibility_facts(&self.inputs, &self.skills)
    }

    pub fn into_parts(self) -> (Vec<ResolvedInput>, Vec<ResolvedSkillBinding>) {
        (self.inputs, self.skills)
    }

    /// Project the frozen inputs back to the neutral binding vocabulary used by
    /// the pure Sandbox-layout preflight. Configuration versions and credential
    /// pins cannot affect layout and therefore remain exclusively in the
    /// resolved manifest.
    #[must_use]
    pub fn sandbox_layout_bindings(&self) -> Vec<InputBinding> {
        self.inputs
            .iter()
            .map(|input| InputBinding {
                binding_id: input.binding_id.clone(),
                target: match &input.source {
                    ResolvedInputSource::File { file_id } => InputResourceId::File(file_id.clone()),
                    ResolvedInputSource::MemoryStore {
                        memory_store_id, ..
                    } => InputResourceId::MemoryStore(memory_store_id.clone()),
                    ResolvedInputSource::Repository { repository_id, .. } => {
                        InputResourceId::Repository(repository_id.clone())
                    }
                },
                mount_path: input.mount_path.clone(),
                access: input.access,
                instructions: input.instructions.clone(),
            })
            .collect()
    }

    /// Whether two complete manifests differ only in File bindings.
    ///
    /// Interactive profiled Sessions use this pure comparison before the
    /// canonical manifest CAS. Binding identity, not vector position, owns the
    /// comparison; every non-File input and every exact Skill pin remains
    /// immutable while File bindings may be attached, replaced, or removed.
    #[must_use]
    pub(crate) fn preserves_profiled_resources_except_files(&self, next: &Self) -> bool {
        fn protected_inputs(
            resources: &ResolvedSessionResources,
        ) -> std::collections::BTreeMap<&str, &ResolvedInput> {
            resources
                .inputs
                .iter()
                .filter(|input| !matches!(input.source, ResolvedInputSource::File { .. }))
                .map(|input| (input.binding_id.as_str(), input))
                .collect()
        }

        protected_inputs(self) == protected_inputs(next)
            && self.skills.len() == next.skills.len()
            && self
                .skills
                .iter()
                .all(|skill| next.skills.iter().any(|candidate| candidate == skill))
    }

    /// Replace the complete resolved Skill set without opening the input
    /// collection or allowing duplicate/invalid pins to be installed.
    pub fn with_skills(
        mut self,
        skills: Vec<ResolvedSkillBinding>,
    ) -> Result<Self, SessionInputError> {
        validate_resolved_skills(&skills)?;
        self.skills = skills;
        Ok(self)
    }

    /// Add one already-resolved input while preserving unique binding ids and
    /// normalized, collision-free mount paths.
    pub fn attach(&self, input: ResolvedInput) -> Result<Self, SessionInputError> {
        let id = input.binding_id.as_str();
        if id.trim().is_empty()
            || self
                .inputs
                .iter()
                .any(|current| current.binding_id == input.binding_id)
        {
            return Err(SessionInputError::InvalidBindingId(id.into()));
        }
        let mut next = self.clone();
        next.inputs.push(input);
        validate_resolved_inputs(&mut next.inputs, RepositoryPathPolicy::Current)?;
        Ok(next)
    }

    /// Replace one live binding without re-resolving any unaffected input.
    pub fn replace(&self, input: ResolvedInput) -> Result<Self, SessionInputError> {
        let mut next = self.clone();
        let current = next
            .inputs
            .iter_mut()
            .find(|current| current.binding_id == input.binding_id)
            .ok_or_else(|| SessionInputError::UnknownBinding(input.binding_id.to_string()))?;
        *current = input;
        validate_resolved_inputs(&mut next.inputs, RepositoryPathPolicy::Current)?;
        Ok(next)
    }

    /// Transform one binding through a closed copy-on-write boundary. The
    /// candidate is revalidated before it replaces the current manifest, so a
    /// caller can update source-specific pins without receiving mutable access
    /// to identity or mount invariants.
    pub fn update_input(
        &self,
        binding_id: &BindingId,
        update: impl FnOnce(&mut ResolvedInput),
    ) -> Result<Self, SessionInputError> {
        let mut input = self
            .inputs
            .iter()
            .find(|input| &input.binding_id == binding_id)
            .cloned()
            .ok_or_else(|| SessionInputError::UnknownBinding(binding_id.to_string()))?;
        update(&mut input);
        if &input.binding_id != binding_id {
            return Err(SessionInputError::InvalidBindingId(
                input.binding_id.to_string(),
            ));
        }
        self.replace(input)
    }

    /// Remove one live binding, returning both the new manifest and removed input.
    pub fn detach(
        &self,
        binding_id: &BindingId,
    ) -> Result<(Self, ResolvedInput), SessionInputError> {
        let mut next = self.clone();
        let index = next
            .inputs
            .iter()
            .position(|input| &input.binding_id == binding_id)
            .ok_or_else(|| SessionInputError::UnknownBinding(binding_id.to_string()))?;
        let removed = next.inputs.remove(index);
        Ok((next, removed))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SessionInputError {
    #[error("binding id `{0}` is empty or duplicated")]
    InvalidBindingId(String),
    #[error("unsafe resource mount path `{0}`")]
    UnsafeMountPath(String),
    #[error("multiple resources claim mount path `{0}` without one explicit Session replacement")]
    MountCollision(String),
    #[error("Session attachment replaces unknown Agent binding `{0}`")]
    UnknownReplacement(String),
    #[error("Session input binding `{0}` was not found")]
    UnknownBinding(String),
    #[error("invalid Repository credential execution pin: {0}")]
    InvalidCredentialPin(String),
    #[error("invalid resolved Skill pin: {0}")]
    InvalidSkillPin(String),
    #[error("invalid Session Resource transition: {0}")]
    InvalidTransition(String),
    #[error(transparent)]
    Registry(#[from] awaken_resource_contract::ResourceRegistryError),
}

fn normalized_mount(path: &str) -> Result<String, SessionInputError> {
    if path.contains('\0') {
        return Err(SessionInputError::UnsafeMountPath(path.into()));
    }
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if components.is_empty()
        || components
            .iter()
            .any(|component| matches!(*component, "." | ".."))
    {
        return Err(SessionInputError::UnsafeMountPath(path.into()));
    }
    Ok(format!("/{}", components.join("/")))
}

fn validate_current_repository_mounts(repository_paths: &[&str]) -> Result<(), SessionInputError> {
    awaken_provisioning_contract::validate_repository_mount_paths(repository_paths, &[])
        .map_err(|error| SessionInputError::UnsafeMountPath(error.to_string()))
}

const LEGACY_REPOSITORY_MOUNT_PATH: &str = "/repo";

fn validate_replayed_repository_mounts(repository_paths: &[&str]) -> Result<(), SessionInputError> {
    let mut legacy_seen = false;
    let mut current = Vec::with_capacity(repository_paths.len());
    for path in repository_paths {
        if *path == LEGACY_REPOSITORY_MOUNT_PATH {
            if legacy_seen {
                return Err(SessionInputError::UnsafeMountPath(format!(
                    "repository mount path {path:?} overlaps another resource tree"
                )));
            }
            legacy_seen = true;
        } else {
            current.push(*path);
        }
    }
    // `/repo` is disjoint from every current path because the current grammar
    // admits only children of `/workspace`. The canonical validator therefore
    // remains the sole owner for every current path and their tree overlaps.
    validate_current_repository_mounts(&current)
}

fn validate_bindings(bindings: &mut [InputBinding]) -> Result<(), SessionInputError> {
    let mut ids = HashSet::new();
    let mut file_paths = HashSet::new();
    let mut memory_paths = HashSet::new();
    let mut repository_paths = Vec::new();
    for binding in bindings.iter_mut() {
        let id = binding.binding_id.as_str();
        if id.trim().is_empty() || !ids.insert(id.to_string()) {
            return Err(SessionInputError::InvalidBindingId(id.into()));
        }
        // Only equal logical paths inside one typed projection class can be
        // decided here. File, Memory, and Repository use different final path
        // projections; the provider-effective Runtime layout owns every
        // cross-class and projected ancestor/descendant collision.
        let paths = match &binding.target {
            InputResourceId::File(_) => {
                binding.mount_path = normalized_mount(&binding.mount_path)?;
                Some(&mut file_paths)
            }
            InputResourceId::MemoryStore(_) => {
                binding.mount_path = normalized_mount(&binding.mount_path)?;
                Some(&mut memory_paths)
            }
            InputResourceId::Repository(_) => None,
        };
        if paths.is_some_and(|paths| !paths.insert(binding.mount_path.clone())) {
            return Err(SessionInputError::MountCollision(
                binding.mount_path.clone(),
            ));
        }
    }
    repository_paths.extend(bindings.iter().filter_map(|binding| {
        matches!(&binding.target, InputResourceId::Repository(_))
            .then_some(binding.mount_path.as_str())
    }));
    validate_current_repository_mounts(&repository_paths)
}

#[derive(Clone, Copy)]
enum RepositoryPathPolicy {
    Current,
    DurableReplay,
}

fn validate_resolved_inputs(
    inputs: &mut [ResolvedInput],
    repository_policy: RepositoryPathPolicy,
) -> Result<(), SessionInputError> {
    let mut ids = HashSet::new();
    let mut file_paths = HashSet::new();
    let mut memory_paths = HashSet::new();
    let mut repository_paths = Vec::new();
    for input in inputs.iter_mut() {
        let id = input.binding_id.as_str();
        if id.trim().is_empty() || !ids.insert(id.to_string()) {
            return Err(SessionInputError::InvalidBindingId(id.into()));
        }
        let paths = match &input.source {
            ResolvedInputSource::File { .. } => {
                input.mount_path = normalized_mount(&input.mount_path)?;
                Some(&mut file_paths)
            }
            ResolvedInputSource::MemoryStore { .. } => {
                input.mount_path = normalized_mount(&input.mount_path)?;
                Some(&mut memory_paths)
            }
            ResolvedInputSource::Repository { .. } => None,
        };
        if paths.is_some_and(|paths| !paths.insert(input.mount_path.clone())) {
            return Err(SessionInputError::MountCollision(input.mount_path.clone()));
        }
    }
    repository_paths.extend(inputs.iter().filter_map(|input| {
        matches!(&input.source, ResolvedInputSource::Repository { .. })
            .then_some(input.mount_path.as_str())
    }));
    match repository_policy {
        RepositoryPathPolicy::Current => validate_current_repository_mounts(&repository_paths),
        RepositoryPathPolicy::DurableReplay => {
            validate_replayed_repository_mounts(&repository_paths)
        }
    }
}

fn validate_resolved_skills(skills: &[ResolvedSkillBinding]) -> Result<(), SessionInputError> {
    if skills.len() > 500 {
        return Err(SessionInputError::InvalidSkillPin(
            "at most 500 Skills may be selected".into(),
        ));
    }
    let mut identities = std::collections::BTreeSet::new();
    for skill in skills {
        if skill.skill_id.trim().is_empty() {
            return Err(SessionInputError::InvalidSkillPin(
                "Skill id must be non-empty".into(),
            ));
        }
        if skill.version == 0 {
            return Err(SessionInputError::InvalidSkillPin(format!(
                "Skill `{}` has version zero",
                skill.skill_id
            )));
        }
        if skill.bundle_sha256.trim().is_empty() {
            return Err(SessionInputError::InvalidSkillPin(format!(
                "Skill `{}` has an empty bundle digest",
                skill.skill_id
            )));
        }
        if !identities.insert((skill.kind, skill.skill_id.as_str())) {
            return Err(SessionInputError::InvalidSkillPin(format!(
                "Skill `{}` is duplicated",
                skill.skill_id
            )));
        }
    }
    Ok(())
}

impl SessionInputResolver {
    /// Derive the effective bindings from typed Agent defaults and Session
    /// attachments. Only an explicit `replaces` removes an Agent binding; all
    /// remaining id/path collisions fail.
    pub fn effective_bindings(
        agent_defaults: &[InputBinding],
        session_attachments: &[SessionInputAttachment],
    ) -> Result<Vec<InputBinding>, SessionInputError> {
        let mut defaults = agent_defaults.to_vec();
        validate_bindings(&mut defaults)?;
        let mut attachments = session_attachments
            .iter()
            .map(|attachment| attachment.binding.clone())
            .collect::<Vec<_>>();
        validate_bindings(&mut attachments)?;

        let default_ids = defaults
            .iter()
            .map(|binding| binding.binding_id.clone())
            .collect::<HashSet<_>>();
        let mut replaced = HashSet::new();
        for attachment in session_attachments {
            if let Some(binding_id) = &attachment.replaces
                && default_ids.contains(binding_id)
            {
                replaced.insert(binding_id.clone());
            } else if let Some(binding_id) = &attachment.replaces {
                return Err(SessionInputError::UnknownReplacement(
                    binding_id.to_string(),
                ));
            }
        }

        let mut effective = attachments;
        effective.extend(
            defaults
                .into_iter()
                .filter(|binding| !replaced.contains(&binding.binding_id)),
        );
        validate_bindings(&mut effective)?;
        Ok(effective)
    }

    /// Derive and resolve the current Memory/Repository configuration exactly
    /// once. The caller supplies a trusted Workspace after the edge PEP has made
    /// its authorization decision; this method contains no authorization policy.
    pub fn resolve_inputs(
        workspace_id: &str,
        catalog: Option<&dyn ExecutionResourceResolver>,
        agent_defaults: &[InputBinding],
        session_attachments: &[SessionInputAttachment],
    ) -> Result<ResolvedSessionResources, SessionInputError> {
        let inputs = Self::effective_bindings(agent_defaults, session_attachments)?
            .into_iter()
            .map(|binding| {
                let source = match binding.target {
                    InputResourceId::File(file_id) => ResolvedInputSource::File { file_id },
                    InputResourceId::MemoryStore(memory_store_id) => {
                        let config = catalog
                            .ok_or_else(|| {
                                ResourceRegistryError::NotFound(memory_store_id.to_string())
                            })?
                            .resolve_memory_store(workspace_id, memory_store_id.as_str())?;
                        ResolvedInputSource::MemoryStore {
                            memory_store_id,
                            config,
                        }
                    }
                    InputResourceId::Repository(repository_id) => {
                        let config = catalog
                            .ok_or_else(|| {
                                ResourceRegistryError::NotFound(repository_id.to_string())
                            })?
                            .resolve_repository(workspace_id, repository_id.as_str())?;
                        ResolvedInputSource::Repository {
                            repository_id,
                            config,
                            credential: None,
                        }
                    }
                };
                let access = if matches!(&source, ResolvedInputSource::File { .. }) {
                    awaken_resource_contract::ResourceAccess::ReadOnly
                } else {
                    binding.access
                };
                Ok(ResolvedInput {
                    binding_id: binding.binding_id,
                    source,
                    mount_path: binding.mount_path,
                    access,
                    instructions: binding.instructions,
                })
            })
            .collect::<Result<Vec<_>, SessionInputError>>()?;
        ResolvedSessionResources::try_new(inputs, Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use awaken_resource_contract::{
        ClonePolicy, ConfigVersion, ExecutionResourceResolver, FileId, MemoryStoreId, RepositoryId,
        ResourceRegistryError, RetentionPolicy,
    };

    #[derive(Default)]
    struct Registry {
        memory_resolves: AtomicUsize,
        repository_resolves: AtomicUsize,
    }

    impl ExecutionResourceResolver for Registry {
        fn resolve_memory_store(
            &self,
            workspace_id: &str,
            id: &str,
        ) -> Result<MemoryStoreConfigVersion, ResourceRegistryError> {
            self.memory_resolves.fetch_add(1, Ordering::Relaxed);
            if workspace_id != "workspace-a" || id != "memory-1" {
                return Err(ResourceRegistryError::NotFound(id.into()));
            }
            Ok(MemoryStoreConfigVersion {
                memory_store_id: id.into(),
                version: ConfigVersion(4),
                retention_policy: RetentionPolicy::default(),
            })
        }

        fn resolve_repository(
            &self,
            workspace_id: &str,
            id: &str,
        ) -> Result<RepositoryConfigVersion, ResourceRegistryError> {
            self.repository_resolves.fetch_add(1, Ordering::Relaxed);
            if workspace_id != "workspace-a" || id != "repo-1" {
                return Err(ResourceRegistryError::NotFound(id.into()));
            }
            Ok(RepositoryConfigVersion {
                repository_id: id.into(),
                version: ConfigVersion(7),
                remote_url: "https://example.test/repo.git".into(),
                credential_binding: Some("credential-1".into()),
                initial_branch: Some("main".into()),
                initial_commit: None,
                clone_policy: ClonePolicy::default(),
            })
        }
    }

    /// Repository pin cause/effect graph: C1 source id matches; C2 the pin has
    /// one target equal to the normalized HTTPS remote origin; C3 usage is HTTP
    /// Basic; C4 selected holder is allowed; C5 model exposure is forbidden.
    /// E1 exact pins and same-origin repository paths are admitted; E2 any
    /// missing/mismatched/unsafe authority fact is rejected before material or
    /// Git I/O.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | C5 | Effect |
    /// |---|---|---|---|---|---|---|
    /// | P1 | T | T | T | T | T | E1 |
    /// | P2 | F | - | - | - | - | E2 |
    /// | P3 | T | F | - | - | - | E2 |
    /// | P4 | T | T | F | - | - | E2 |
    /// | P5 | T | T | T | F | - | E2 |
    /// | P6 | T | T | T | T | F | E2 |
    #[test]
    fn repository_credential_pin_is_bound_to_one_https_origin() {
        let holder = awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            "spiffe://example.test/worker",
        );
        let exact_target =
            repository_transport_credential_target("https://github.com/awaken/first.git")
                .expect("P1 target");
        let access = awaken_credential_contract::CredentialAccess::new(
            awaken_credential_contract::CredentialRef {
                id: "credential-1".into(),
                revision: 1,
            },
            awaken_credential_contract::CredentialMaterialSource::ControlPlaneReference,
            repository_transport_credential_usage(),
            awaken_credential_contract::CredentialExecutionPolicy::exact(
                holder.clone(),
                awaken_credential_contract::ModelExposurePolicy::Forbidden,
            ),
        )
        .with_target(exact_target.clone());
        let exact = ResolvedRepositoryCredential {
            access,
            selected_plaintext_holder: holder.clone(),
        };

        assert!(
            exact
                .validate_for_repository("credential-1", "https://github.com/awaken/second.git",)
                .is_ok(),
            "P1/E1 same origin"
        );
        assert!(
            exact
                .validate_for_repository("credential-2", "https://github.com/awaken/first.git")
                .is_err(),
            "P2/E2"
        );

        let mut missing_target = exact.clone();
        missing_target.access.target = None;
        assert!(
            missing_target
                .validate_for_repository("credential-1", "https://github.com/awaken/first.git")
                .is_err(),
            "P3/E2 missing target"
        );
        assert!(
            exact
                .validate_for_repository(
                    "credential-1",
                    "https://git.example.test/awaken/first.git",
                )
                .is_err(),
            "P3/E2 other origin"
        );
        assert!(
            exact
                .validate_for_repository("credential-1", "http://github.com/awaken/first.git")
                .is_err(),
            "P3/E2 insecure remote"
        );

        let mut wrong_usage = exact.clone();
        wrong_usage.access.usage = awaken_credential_contract::CredentialUsage::HttpHeader {
            name: "authorization".into(),
            scheme: Some("Bearer".into()),
        };
        assert!(
            wrong_usage
                .validate_for_repository("credential-1", "https://github.com/awaken/first.git")
                .is_err(),
            "P4/E2"
        );

        let mut wrong_holder = exact.clone();
        wrong_holder.selected_plaintext_holder = awaken_credential_contract::PlaintextHolder::new(
            awaken_credential_contract::PlaintextBoundary::Worker,
            "spiffe://example.test/other-worker",
        );
        assert!(
            wrong_holder
                .validate_for_repository("credential-1", "https://github.com/awaken/first.git")
                .is_err(),
            "P5/E2"
        );

        let mut exposed = exact;
        exposed.access.policy.model_exposure =
            awaken_credential_contract::ModelExposurePolicy::VirtualOnly;
        assert!(
            exposed
                .validate_for_repository("credential-1", "https://github.com/awaken/first.git")
                .is_err(),
            "P6/E2"
        );
    }

    /// Material-binding cause/effect graph: C1=Workspace identity;
    /// C2=Repository identity; C3=frozen config version; C4=canonical
    /// Repository transport usage. E1 the Session compiler and deployment
    /// materializer derive one identical binding; E2 changing any durable
    /// identity axis changes the binding before credential material is opened.
    ///
    /// | Rule | C1 | C2 | C3 | C4 | Effect |
    /// |---|---|---|---|---|---|
    /// | B1 | exact | exact | exact | exact | E1 |
    /// | B2 | changed | exact | exact | exact | E2 |
    /// | B3 | exact | changed | exact | exact | E2 |
    /// | B4 | exact | exact | changed | exact | E2 |
    #[test]
    fn repository_material_binding_has_one_durable_authority() {
        let repository_id = RepositoryId::new("repository-a");
        let exact =
            repository_transport_material_binding("workspace-a", &repository_id, ConfigVersion(7));
        assert_eq!(
            exact,
            awaken_credential_contract::CredentialMaterialBinding::for_target(
                "workspace-a",
                &(&repository_id, ConfigVersion(7)),
                &repository_transport_credential_usage(),
            ),
            "B1/E1",
        );
        assert_ne!(
            exact,
            repository_transport_material_binding("workspace-b", &repository_id, ConfigVersion(7),),
            "B2/E2",
        );
        assert_ne!(
            exact,
            repository_transport_material_binding(
                "workspace-a",
                &RepositoryId::new("repository-b"),
                ConfigVersion(7),
            ),
            "B3/E2",
        );
        assert_ne!(
            exact,
            repository_transport_material_binding("workspace-a", &repository_id, ConfigVersion(8),),
            "B4/E2",
        );
    }

    fn binding(
        binding_id: &str,
        target: InputResourceId,
        mount_path: &str,
        access: awaken_resource_contract::ResourceAccess,
    ) -> InputBinding {
        InputBinding {
            binding_id: BindingId::from(binding_id),
            target,
            mount_path: mount_path.into(),
            access,
            instructions: None,
        }
    }

    #[test]
    fn typed_attachment_replacement_never_infers_cross_projection_collisions() {
        // Typed logical-path decision table: C1 Memory and File share one raw
        // spelling but project below different Runtime roots -> E1 coexist;
        // C2 the caller explicitly replaces the Memory binding with the File ->
        // E2 preserve that explicit replacement. Same-class duplicates remain
        // rejected by `validate_bindings`; cross-class final aliases are owned
        // by the provider-effective Sandbox-layout gate.
        let defaults = vec![binding(
            "memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
            "/mnt/context",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        )];
        let file = binding(
            "file",
            InputResourceId::File(FileId::from("file-1")),
            "mnt/context",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        );
        let coexisting = SessionInputResolver::effective_bindings(
            &defaults,
            &[SessionInputAttachment {
                binding: file.clone(),
                replaces: None,
            }],
        )
        .expect("C1/E1");
        assert_eq!(coexisting.len(), 2, "C1/E1");

        let effective = SessionInputResolver::resolve_inputs(
            "workspace-a",
            Some(&Registry::default()),
            &defaults,
            &[SessionInputAttachment {
                binding: file,
                replaces: Some(BindingId::from("memory")),
            }],
        )
        .expect("C2/E2");
        assert_eq!(effective.inputs.len(), 1, "C2/E2");
        assert_eq!(effective.inputs[0].mount_path, "/mnt/context");
        assert_eq!(
            effective.inputs[0].access,
            awaken_resource_contract::ResourceAccess::ReadOnly
        );
    }

    #[test]
    fn mutable_resources_resolve_once_without_content_pins_or_authorization() {
        let registry = Registry::default();
        let defaults = vec![
            binding(
                "memory",
                InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
                "/mnt/memory",
                awaken_resource_contract::ResourceAccess::ReadOnly,
            ),
            binding(
                "repo",
                InputResourceId::Repository(RepositoryId::from("repo-1")),
                "/workspace/repo",
                awaken_resource_contract::ResourceAccess::ReadWrite,
            ),
        ];

        let effective =
            SessionInputResolver::resolve_inputs("workspace-a", Some(&registry), &defaults, &[])
                .unwrap();

        assert_eq!(registry.memory_resolves.load(Ordering::Relaxed), 1);
        assert_eq!(registry.repository_resolves.load(Ordering::Relaxed), 1);
        let wire = serde_json::to_string(&effective).unwrap();
        assert!(wire.contains("\"version\":4"));
        assert!(wire.contains("\"version\":7"));
        for forbidden in ["commit", "tree", "head", "principal", "policy_decision"] {
            assert!(
                !wire.contains(forbidden),
                "unexpected `{forbidden}` in {wire}"
            );
        }
    }

    /// Repository path lifecycle cause/effect graph: C1 current ingress or
    /// `try_new` carries one canonical `/workspace` child; C2 it carries a
    /// relative path, another root, or a canonicalization alias; C3 durable
    /// serde replay carries the one historical `/repo` spelling; C4 either
    /// current constructor has overlapping Repository trees. Effects: E1 admit
    /// C1 without rewriting; E2 reject C2 at the current boundary and reject an
    /// alias at the durable boundary; E3 decode C3 byte-for-byte through the
    /// private storage-only compatibility path; E4 reject C4 as a whole set.
    /// File/Memory authored paths are excluded because their final projections
    /// live under separate Runtime-owned roots.
    ///
    /// | Rule | Boundary | Repository bytes | Other tree | Effect |
    /// |---|---|---|---|---|
    /// | R1 | current | `/workspace/repo` | disjoint | E1 |
    /// | R2 | current/durable | alias or unsafe | - | E2 |
    /// | R3 | durable only | `/repo` | disjoint | E3 |
    /// | R4 | current | canonical | overlaps | E4 |
    #[test]
    fn new_repository_paths_fail_closed_without_rewriting_historical_values() {
        let registry = Registry::default();
        let repository = |mount_path: &str| {
            binding(
                "repo",
                InputResourceId::Repository(RepositoryId::from("repo-1")),
                mount_path,
                awaken_resource_contract::ResourceAccess::ReadWrite,
            )
        };

        let canonical = SessionInputResolver::resolve_inputs(
            "workspace-a",
            Some(&registry),
            &[repository("/workspace/repo")],
            &[],
        )
        .expect("C1/E1");
        for mount_path in [
            "/repo",
            "repo",
            "/workspace//repo",
            "/workspace/repo/",
            "/workspace/repo\\child",
            "/workspace/repo\nchild",
            "/workspace/.skills/repo",
        ] {
            let error = SessionInputResolver::resolve_inputs(
                "workspace-a",
                Some(&registry),
                &[repository(mount_path)],
                &[],
            )
            .expect_err("C2/E2");
            assert!(matches!(error, SessionInputError::UnsafeMountPath(_)));
        }
        assert_eq!(
            registry.repository_resolves.load(Ordering::Relaxed),
            1,
            "C2 rejects before catalog resolution"
        );
        let overlapping_repository = binding(
            "nested-repo",
            InputResourceId::Repository(RepositoryId::from("repo-1")),
            "/workspace/repo/README.md",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        );
        assert!(matches!(
            SessionInputResolver::resolve_inputs(
                "workspace-a",
                Some(&registry),
                &[repository("/workspace/repo"), overlapping_repository],
                &[],
            ),
            Err(SessionInputError::UnsafeMountPath(_))
        ));
        assert_eq!(
            registry.repository_resolves.load(Ordering::Relaxed),
            1,
            "C4/E4 rejects before Repository resolution"
        );

        for mount_path in ["/repo", "/workspace//repo"] {
            let mut current = canonical.inputs().to_vec();
            current[0].mount_path = mount_path.into();
            assert!(
                ResolvedSessionResources::try_new(current, canonical.skills().to_vec()).is_err(),
                "R2/E2 public construction must reject {mount_path:?}"
            );
        }

        let mut overlap = canonical.inputs().to_vec();
        let mut nested = overlap[0].clone();
        nested.binding_id = BindingId::from("nested-repo");
        nested.mount_path = "/workspace/repo/child".into();
        overlap.push(nested);
        assert!(
            ResolvedSessionResources::try_new(overlap, canonical.skills().to_vec()).is_err(),
            "R4/E4 direct construction validates the complete Repository set"
        );

        let mut alias_wire = serde_json::to_value(&canonical).unwrap();
        alias_wire["inputs"][0]["mount_path"] = serde_json::json!("/workspace//repo");
        assert!(
            serde_json::from_value::<ResolvedSessionResources>(alias_wire).is_err(),
            "R2/E2 durable aliases are rejected rather than rewritten"
        );

        let mut legacy_wire = serde_json::to_value(&canonical).unwrap();
        legacy_wire["inputs"][0]["mount_path"] = serde_json::json!("/repo");
        let reloaded: ResolvedSessionResources =
            serde_json::from_value(legacy_wire).expect("R3 storage-only legacy decode");
        assert_eq!(reloaded.inputs()[0].mount_path, "/repo", "R3/E3");
    }

    #[test]
    fn unsafe_path_and_cross_workspace_fail_closed() {
        let unsafe_binding = binding(
            "file",
            InputResourceId::File(FileId::from("file-1")),
            "/workspace/../secret",
            awaken_resource_contract::ResourceAccess::ReadOnly,
        );
        assert!(matches!(
            SessionInputResolver::effective_bindings(&[unsafe_binding], &[]),
            Err(SessionInputError::UnsafeMountPath(_))
        ));

        let memory_binding = binding(
            "memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
            "/mnt/memory",
            awaken_resource_contract::ResourceAccess::ReadOnly,
        );
        assert!(matches!(
            SessionInputResolver::resolve_inputs(
                "workspace-b",
                Some(&Registry::default()),
                &[memory_binding],
                &[]
            ),
            Err(SessionInputError::Registry(
                ResourceRegistryError::NotFound(_)
            ))
        ));
    }

    #[test]
    fn file_inputs_need_no_catalog_but_configured_resources_fail_closed_without_one() {
        let file_binding = binding(
            "file",
            InputResourceId::File(FileId::from("file-1")),
            "/mnt/file",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        );
        let resolved =
            SessionInputResolver::resolve_inputs("workspace-a", None, &[file_binding], &[])
                .unwrap();
        assert_eq!(resolved.inputs.len(), 1);
        assert_eq!(
            resolved.inputs[0].access,
            awaken_resource_contract::ResourceAccess::ReadOnly
        );

        for (target, mount_path) in [
            (
                InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
                "/mnt/configured",
            ),
            (
                InputResourceId::Repository(RepositoryId::from("repo-1")),
                "/workspace/configured",
            ),
        ] {
            let configured = binding(
                "configured",
                target,
                mount_path,
                awaken_resource_contract::ResourceAccess::ReadOnly,
            );
            assert!(matches!(
                SessionInputResolver::resolve_inputs("workspace-a", None, &[configured], &[]),
                Err(SessionInputError::Registry(
                    ResourceRegistryError::NotFound(_)
                ))
            ));
        }
    }

    fn resolved_file(binding_id: &str, file_id: &str, mount_path: &str) -> ResolvedInput {
        ResolvedInput {
            binding_id: BindingId::from(binding_id),
            source: ResolvedInputSource::File {
                file_id: FileId::from(file_id),
            },
            mount_path: mount_path.into(),
            access: awaken_resource_contract::ResourceAccess::ReadOnly,
            instructions: None,
        }
    }

    /// Resource capability cause/effect graph: C1 the retained generation is
    /// empty; C2 it has only a Skill; C3 it has a read-only File; C4 it has a
    /// Repository whose frozen config selects a credential binding. E1 one
    /// canonical projector reports whether any Resource plane is needed; E2
    /// Repository, read-only, and credential axes remain independent. Host Run
    /// placement and terminal-cleanup admission consume these same facts.
    ///
    /// | Rule | inputs/skills | resources | repo | read-only | repo credential |
    /// |---|---|---|---|---|---|
    /// | F1 | empty | F | F | F | F |
    /// | F2 | Skill | T | F | F | F |
    /// | F3 | read-only File | T | F | T | F |
    /// | F4 | credentialed Repository | T | T | F | T |
    #[test]
    fn resource_compatibility_facts_have_one_typed_projector() {
        let empty = ResolvedSessionResources::default().compatibility_facts();
        assert_eq!(empty, SessionResourceCompatibilityFacts::default(), "F1");

        let skill_only = ResolvedSessionResources::try_new(
            Vec::new(),
            vec![ResolvedSkillBinding {
                kind: awaken_agent_contract::AgentSkillKind::Custom,
                skill_id: "review".into(),
                version: 1,
                bundle_sha256: "sha-review-1".into(),
            }],
        )
        .expect("F2 Skill manifest")
        .compatibility_facts();
        assert!(skill_only.has_resources(), "F2/resources");
        assert!(!skill_only.has_repository(), "F2/repository");
        assert!(!skill_only.has_readonly_input(), "F2/read-only");
        assert!(!skill_only.has_credentialed_repository(), "F2/credential");

        let file = ResolvedSessionResources::try_new(
            vec![resolved_file("file", "file-1", "/mnt/file")],
            Vec::new(),
        )
        .expect("F3 File manifest")
        .compatibility_facts();
        assert!(file.has_resources(), "F3/resources");
        assert!(!file.has_repository(), "F3/repository");
        assert!(file.has_readonly_input(), "F3/read-only");
        assert!(!file.has_credentialed_repository(), "F3/credential");

        let repository = SessionInputResolver::resolve_inputs(
            "workspace-a",
            Some(&Registry::default()),
            &[binding(
                "repository",
                InputResourceId::Repository(RepositoryId::from("repo-1")),
                "/workspace/repository",
                awaken_resource_contract::ResourceAccess::ReadWrite,
            )],
            &[],
        )
        .expect("F4 Repository manifest")
        .compatibility_facts();
        assert!(repository.has_resources(), "F4/resources");
        assert!(repository.has_repository(), "F4/repository");
        assert!(!repository.has_readonly_input(), "F4/read-only");
        assert!(repository.has_credentialed_repository(), "F4/credential");
    }

    #[test]
    fn profiled_resource_policy_compares_complete_manifests_by_authoritative_kind() {
        // Cause/effect graph: C1 policy Managed/Frozen/FileResources; C2 the
        // candidate changes only File bindings; C3 it changes a non-File input;
        // C4 it changes an exact Skill pin. Effects: E1 Managed admits every
        // valid manifest; E2 Frozen rejects even an unchanged new command; E3
        // FileResources admits C2 but rejects C3/C4. Constraint: non-File
        // identity is keyed by binding id, never vector position.
        // Decision rules R1=Managed=>E1, R2=Frozen=>E2,
        // R3=FileResources+C2=>admit, R4/R5=FileResources+(C3|C4)=>reject.
        let protected = binding(
            "memory",
            InputResourceId::MemoryStore(MemoryStoreId::from("memory-1")),
            "/mnt/memory",
            awaken_resource_contract::ResourceAccess::ReadWrite,
        );
        let current = SessionInputResolver::resolve_inputs(
            "workspace-a",
            Some(&Registry::default()),
            &[protected],
            &[],
        )
        .expect("resolved protected input")
        .with_skills(vec![ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "review".into(),
            version: 7,
            bundle_sha256: "sha-review-7".into(),
        }])
        .expect("exact Skill pin")
        .attach(resolved_file("file", "file-a", "/mnt/file"))
        .expect("initial File");
        let file_only = current
            .replace(resolved_file("file", "file-b", "/mnt/file"))
            .expect("R3 File replacement");
        let mut changed_non_file_inputs = current.inputs().to_vec();
        changed_non_file_inputs
            .iter_mut()
            .find(|input| input.binding_id.as_str() == "memory")
            .expect("protected input")
            .instructions = Some("changed".into());
        let changed_non_file =
            ResolvedSessionResources::try_new(changed_non_file_inputs, current.skills().to_vec())
                .expect("valid non-File candidate");
        let changed_skill = ResolvedSessionResources::try_new(
            current.inputs().to_vec(),
            vec![ResolvedSkillBinding {
                version: 8,
                bundle_sha256: "sha-review-8".into(),
                ..current.skills()[0].clone()
            }],
        )
        .expect("valid Skill candidate");

        assert!(
            crate::SessionMutationPolicy::Managed
                .admits_resource_replacement(&current, &changed_non_file),
            "R1/E1"
        );
        assert!(
            !crate::SessionMutationPolicy::Frozen.admits_resource_replacement(&current, &current),
            "R2/E2"
        );
        assert!(
            crate::SessionMutationPolicy::FileResources
                .admits_resource_replacement(&current, &file_only),
            "R3/E3"
        );
        assert!(
            !crate::SessionMutationPolicy::FileResources
                .admits_resource_replacement(&current, &changed_non_file),
            "R4/E3"
        );
        assert!(
            !crate::SessionMutationPolicy::FileResources
                .admits_resource_replacement(&current, &changed_skill),
            "R5/E3"
        );
    }

    #[test]
    fn effective_manifest_mutations_preserve_identity_and_invariants() {
        let initial = ResolvedSessionResources::default()
            .attach(resolved_file("input-a", "file-a", "mnt/a"))
            .unwrap();
        assert_eq!(initial.inputs[0].mount_path, "/mnt/a");

        let duplicate_path = initial.attach(resolved_file("input-b", "file-b", "/mnt/a"));
        assert!(matches!(
            duplicate_path,
            Err(SessionInputError::MountCollision(_))
        ));

        let mut replacement = resolved_file("input-a", "file-a", "/mnt/renamed");
        replacement.instructions = Some("read this first".into());
        let replaced = initial.replace(replacement).unwrap();
        assert_eq!(replaced.inputs[0].binding_id.as_str(), "input-a");
        assert_eq!(replaced.inputs[0].mount_path, "/mnt/renamed");

        let (detached, removed) = replaced.detach(&BindingId::from("input-a")).unwrap();
        assert!(detached.inputs.is_empty());
        assert_eq!(removed.binding_id.as_str(), "input-a");
        assert!(matches!(
            detached.detach(&BindingId::from("missing")),
            Err(SessionInputError::UnknownBinding(_))
        ));
    }

    #[test]
    fn complete_manifest_constructor_and_serde_share_the_same_invariants() {
        // Decision table: duplicate binding, duplicate normalized mount, invalid
        // Skill pin, and duplicate Skill identity all fail at both Rust and wire
        // construction boundaries; a legal relative mount normalizes once.
        let normalized = ResolvedSessionResources::try_new(
            vec![resolved_file("input-a", "file-a", "mnt/a")],
            Vec::new(),
        )
        .unwrap();
        assert_eq!(normalized.inputs()[0].mount_path, "/mnt/a");

        let invalid_inputs = [
            vec![
                resolved_file("same", "file-a", "/mnt/a"),
                resolved_file("same", "file-b", "/mnt/b"),
            ],
            vec![
                resolved_file("input-a", "file-a", "mnt/shared"),
                resolved_file("input-b", "file-b", "/mnt/shared"),
            ],
        ];
        for inputs in invalid_inputs {
            let wire = serde_json::json!({ "inputs": inputs, "skills": [] });
            assert!(ResolvedSessionResources::try_new(inputs.clone(), Vec::new()).is_err());
            assert!(serde_json::from_value::<ResolvedSessionResources>(wire).is_err());
        }

        let skill = ResolvedSkillBinding {
            kind: awaken_agent_contract::AgentSkillKind::Custom,
            skill_id: "review".into(),
            version: 1,
            bundle_sha256: "sha256-review".into(),
        };
        for skills in [
            vec![ResolvedSkillBinding {
                version: 0,
                ..skill.clone()
            }],
            vec![skill.clone(), skill.clone()],
        ] {
            let wire = serde_json::json!({ "inputs": [], "skills": skills });
            assert!(ResolvedSessionResources::try_new(Vec::new(), skills.clone()).is_err());
            assert!(serde_json::from_value::<ResolvedSessionResources>(wire).is_err());
        }
    }

    #[test]
    fn copy_on_write_input_update_cannot_publish_a_collision() {
        let resources = ResolvedSessionResources::try_new(
            vec![
                resolved_file("input-a", "file-a", "/mnt/a"),
                resolved_file("input-b", "file-b", "/mnt/b"),
            ],
            Vec::new(),
        )
        .unwrap();
        let result = resources.update_input(&BindingId::from("input-b"), |input| {
            input.mount_path = "/mnt/a".into();
        });
        assert!(matches!(result, Err(SessionInputError::MountCollision(_))));
        assert_eq!(resources.inputs()[1].mount_path, "/mnt/b");

        let result = resources.update_input(&BindingId::from("input-b"), |input| {
            input.binding_id = BindingId::from("input-a");
        });
        assert!(matches!(
            result,
            Err(SessionInputError::InvalidBindingId(_))
        ));
    }

    /// Serialization compatibility causes/effects: C1 a pre-generation durable
    /// dispatch omits `revision`; C2 a current dispatch declares it. E1 C1
    /// decodes as legacy revision zero; E2 C2 preserves the exact generation.
    #[test]
    fn session_resource_manifest_revision_is_backward_compatible() {
        let legacy: SessionResourceManifest = serde_json::from_value(serde_json::json!({
            "workspace_id": "workspace-a",
            "resources": { "inputs": [] }
        }))
        .expect("legacy dispatch manifest");
        assert_eq!(legacy.revision, 0);

        let current = SessionResourceManifest::at_revision(
            "workspace-a",
            7,
            ResolvedSessionResources::default(),
        );
        assert_eq!(
            serde_json::from_value::<SessionResourceManifest>(
                serde_json::to_value(&current).expect("serialize current manifest")
            )
            .expect("deserialize current manifest"),
            current
        );
    }
}
