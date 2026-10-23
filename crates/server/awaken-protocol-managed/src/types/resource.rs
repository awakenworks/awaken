// @formal surface requirement=product.public_dto.anti_corruption
// @formal reason: This source implements the repository-controlled public dto product boundary and is
// @formal reason: classified against its product requirement; this link is not a formal-proof claim.
//! Typed inputs shared by Session and Deployment `resources[]`.

use serde::{Deserialize, Deserializer, Serialize};

/// Write-only repository credential admitted by the Anthropic-compatible wire.
///
/// It is deliberately neither serializable nor printable. The state adapter
/// translates it into canonical write-only material before the application seals
/// it through the existing Vault ingress. Dropping this DTO zeroizes its owned
/// token copy; cloned copies retain their own redacted leaf ownership.
///
/// SDK: inline — `BetaManagedAgentsGitHubRepositoryResourceParams.authorization_token` (also `ResourceUpdateParams.authorization_token`).
#[derive(Clone)]
pub struct RepositoryAuthorizationToken(awaken_agent_contract::RedactedString);

impl RepositoryAuthorizationToken {
    #[must_use]
    pub(crate) fn into_credential_material(
        self,
    ) -> awaken_credential_contract::CredentialMaterialInput {
        awaken_credential_contract::CredentialMaterialInput::structured(
            "github",
            awaken_credential_contract::http_basic_material(
                awaken_agent_contract::RedactedString::new("x-access-token"),
                self.0,
            ),
        )
    }
}

impl<'de> Deserialize<'de> for RepositoryAuthorizationToken {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        String::deserialize(deserializer).map(|value| Self(value.into()))
    }
}

impl std::fmt::Debug for RepositoryAuthorizationToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("RepositoryAuthorizationToken(***)")
    }
}

/// SDK: inline — `BetaManagedAgentsMemoryStoreResourceParam.access`.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

/// The official Managed Agents resource union. Repository authorization is a
/// write-only ingress field and unknown fields fail closed.
///
/// SDK: inline — an element of `SessionCreateParams.resources` (GitHub repository, file or memory store resource params).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceInput {
    File {
        file_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access: Option<ResourceAccess>,
    },
    GithubRepository {
        url: String,
        /// Official write-only clone credential. Serialization always omits it,
        /// so Session projections and idempotency snapshots cannot echo material.
        #[serde(default, skip_serializing)]
        authorization_token: Option<RepositoryAuthorizationToken>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        checkout: Option<RepositoryCheckout>,
    },
}

impl ResourceInput {
    /// One-way request-equivalence fingerprint. Repository credentials remain
    /// absent from every persisted/projection DTO but still participate in
    /// idempotency mismatch detection.
    pub(crate) fn idempotency_fingerprint(&self) -> String {
        match self {
            Self::File {
                file_id,
                mount_path,
            } => awaken_agent_contract::stable_fingerprint(&("file", file_id, mount_path)),
            Self::MemoryStore {
                memory_store_id,
                instructions,
                access,
            } => awaken_agent_contract::stable_fingerprint(&(
                "memory_store",
                memory_store_id,
                instructions,
                access,
            )),
            Self::GithubRepository {
                url,
                authorization_token,
                mount_path,
                checkout,
            } => awaken_agent_contract::stable_fingerprint(&(
                "github_repository",
                url,
                authorization_token
                    .as_ref()
                    .map(|token| token.0.expose_secret()),
                mount_path,
                checkout,
            )),
        }
    }
}

/// SDK: inline — `BetaManagedAgentsGitHubRepositoryResourceParams.checkout`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum RepositoryCheckout {
    Branch { name: String },
    Commit { sha: String },
}

/// `BetaManagedAgentsSessionResource` — the closed output union returned from
/// Session create/retrieve and the `sessions.resources` family. It is a pure
/// projection of the typed Session aggregate and never a second resource index.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionResource {
    File {
        id: String,
        created_at: String,
        file_id: String,
        mount_path: String,
        updated_at: String,
    },
    GithubRepository {
        id: String,
        created_at: String,
        mount_path: String,
        updated_at: String,
        url: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        checkout: Option<RepositoryCheckout>,
    },
    MemoryStore {
        memory_store_id: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        access: Option<ResourceAccess>,
        #[serde(skip_serializing_if = "Option::is_none")]
        instructions: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        mount_path: Option<String>,
    },
}

impl SessionResource {
    /// Only File and Repository resources have an official addressable resource
    /// id. Memory stores are immutable create-time attachments in this API.
    #[must_use]
    pub fn id(&self) -> Option<&str> {
        match self {
            Self::File { id, .. } | Self::GithubRepository { id, .. } => Some(id),
            Self::MemoryStore { .. } => None,
        }
    }
}

/// `ResourceAddParams` — the official live-resource endpoint admits only an
/// already-uploaded File. Repository and Memory bindings belong to the Session
/// creation snapshot, so the endpoint cannot create a parallel mutable path.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceAddParams {
    pub file_id: String,
    #[serde(rename = "type")]
    pub kind: FileResourceKind,
    #[serde(default)]
    pub mount_path: Option<String>,
}

/// SDK: inline — `ResourceAddParams.type`.
#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileResourceKind {
    File,
}

impl ResourceAddParams {
    #[must_use]
    pub(crate) fn into_resource_input(self) -> ResourceInput {
        let Self {
            file_id,
            kind: _,
            mount_path,
        } = self;
        ResourceInput::File {
            file_id,
            mount_path,
        }
    }
}

/// `ResourceUpdateParams`.
///
/// Official write-only repository credential rotation body.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceUpdateParams {
    pub authorization_token: RepositoryAuthorizationToken,
}

/// Complete desired Resource set for one Session. Absence from this list is a
/// removal; callers never need to sequence remote delete/add operations.
///
/// SDK: none — Awaken Resource Manifest extension (full-set replace with If-Match); the Managed API has no such endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceManifestReplaceParams {
    pub resources: Vec<ResourceInput>,
}

/// SDK: none — phase of the Awaken Resource Manifest extension.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ResourceManifestPhase {
    Active,
    Applying,
}

/// SDK: none — response of the Awaken Resource Manifest extension.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct SessionResourceManifest {
    pub desired_revision: u64,
    pub applied_revision: u64,
    pub phase: ResourceManifestPhase,
    pub resources: Vec<SessionResource>,
}

/// Read-only Awaken observation; availability is structural, not an IAM grant.
/// SDK: none — Awaken Resource Manifest observation over the existing Session root.
#[derive(Debug, Clone, Serialize)]
pub struct SessionResourceObservation {
    /// Resource mutation watermark for If-Match; not the selected manifest generation.
    pub revision: u64,
    /// A newer attempted generation ended without replacing the selected inputs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unapplied_revision: Option<u64>,
    pub inputs: Vec<SessionInputDetails>,
    pub skills: Vec<crate::session_resource::ResolvedSkillBinding>,
    #[serde(flatten)]
    pub manifest: SessionResourceManifest,
    pub mutations: SessionResourceMutations,
}

/// SDK: none — safe metadata from the selected Awaken input, without credential material.
#[derive(Debug, Clone, Serialize)]
pub struct SessionInputDetails {
    pub binding_id: String,
    pub resource_kind: &'static str,
    pub target_id: String,
    pub mount_path: String,
    pub access: awaken_resource_contract::ResourceAccess,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub configuration_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// SDK: none — structural availability projected from the immutable Awaken Session policy.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub struct SessionResourceMutations {
    pub files: bool,
    pub replace_manifest: bool,
    pub repository_credentials: bool,
}

impl SessionResourceMutations {
    pub(crate) fn observe(
        policy: crate::session_resource::SessionMutationPolicy,
        terminal: bool,
    ) -> Self {
        use crate::session_resource::SessionMutationPolicy;
        Self {
            files: !terminal && !matches!(policy, SessionMutationPolicy::Frozen),
            replace_manifest: !terminal && policy.is_managed(),
            repository_credentials: !terminal && policy.admits_repository_credential_mutation(),
        }
    }
}

/// `BetaManagedAgentsDeleteSessionResource`.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct DeletedSessionResource {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn observation_projects_each_frozen_policy_without_granting_mutations_to_terminal_sessions() {
        // DT-OBS: C1 Managed/Frozen/FileResources; C2 live/terminal.
        // R1 live Managed -> all structural classes; R2 live FileResources ->
        // files only; R3 Frozen or any terminal -> none. This is a read
        // projection: the original command policy and PEP still enforce writes.
        use crate::session_resource::SessionMutationPolicy as P;
        for (policy, live) in [
            (P::Managed, (true, true, true)),
            (P::FileResources, (true, false, false)),
            (P::Frozen, (false, false, false)),
        ] {
            for terminal in [false, true] {
                let view = SessionResourceMutations::observe(policy, terminal);
                let expected = if terminal {
                    (false, false, false)
                } else {
                    live
                };
                assert_eq!(
                    (
                        view.files,
                        view.replace_manifest,
                        view.repository_credentials
                    ),
                    expected
                );
            }
        }
    }

    #[test]
    fn resource_union_accepts_write_only_credentials_and_rejects_unknown_shapes() {
        // JSON resource -> tagged union -> neutral binding -> sandbox realization
        //
        // | known type | required fields | raw/unknown field | admission |
        // |------------|-----------------|-------------------|-----------|
        // | yes        | yes             | no                | accept    |
        // | yes        | yes             | authorization     | accept    |
        // | yes        | misspelled      | no                | reject    |
        // | no         | any             | any               | reject    |
        let valid = json!({
            "type":"github_repository",
            "url":"https://github.com/acme/repo.git",
            "checkout":{"type":"branch", "name":"main"}
        });
        assert!(serde_json::from_value::<ResourceInput>(valid).is_ok());

        let credential = serde_json::from_value::<ResourceInput>(json!({
            "type":"github_repository",
            "url":"https://github.com/acme/repo.git",
            "authorization_token":"secret"
        }))
        .unwrap();
        let serialized = serde_json::to_string(&credential).unwrap();
        assert!(!serialized.contains("authorization_token"));
        assert!(!serialized.contains("secret"));

        for invalid in [
            json!({"type":"github_repository", "url":"https://github.com/acme/repo.git", "credential_binding":"internal"}),
            json!({"type":"file", "id":"file_1"}),
            json!({"type":"vault", "vault_id":"vlt_1"}),
        ] {
            assert!(serde_json::from_value::<ResourceInput>(invalid).is_err());
        }

        assert!(
            serde_json::from_value::<ResourceInput>(json!({
                "type":"memory_store",
                "memory_store_id":"memstore_1",
                "mount_path":"/mnt/private-extension"
            }))
            .is_err(),
            "the official MemoryStore input has no client-authored mount_path"
        );
    }

    #[test]
    fn github_token_translation_owns_only_provider_and_typed_material() {
        // Cause/effect graph: C1 a Managed GitHub token is present/absent;
        // C2 the existing direct rotation or Session-input translation consumes
        // it. G1a=present+direct and G1b=present+input both yield the same github
        // HTTP Basic document, exact password, and redacted Debug. G2=absent
        // input preserves None. The protocol owns x-access-token; the App adds
        // target/usage and calls the same existing Vault ingress later.
        for through_input in [false, true] {
            let material = if through_input {
                let input: ResourceInput = serde_json::from_value(json!({
                    "type": "github_repository",
                    "url": "https://github.com/acme/repo.git",
                    "authorization_token": "github-token" // awaken-allow(secret): deliberate test fixture
                }))
                .expect("G1b wire token");
                let request = input.to_input_request();
                assert!(
                    !format!("{request:?}").contains("github-token"),
                    "G1b redacted input"
                );
                let awaken_session_application::SessionInputTarget::Repository {
                    credential_material: Some(material),
                    ..
                } = request.target
                else {
                    panic!("G1b canonical material must reach the existing App input")
                };
                material
            } else {
                let token: RepositoryAuthorizationToken =
                    serde_json::from_value(json!("github-token")).expect("G1a token");
                token.into_credential_material()
            };
            assert_eq!(material.provider, "github", "G1/E1");
            assert!(
                !format!("{material:?}").contains("github-token"),
                "G1/E3 redacted material"
            );
            let awaken_credential_contract::CredentialPlaintext::Structured(material) =
                material.plaintext
            else {
                panic!("G1/E2 expected structured material")
            };
            assert_eq!(
                material.type_id,
                awaken_credential_contract::HTTP_BASIC_MATERIAL_TYPE,
                "G1/E2"
            );
            assert_eq!(material.fields.len(), 2, "G1/E2 closed HTTP Basic shape");
            assert_eq!(
                material.fields["username"].expose_secret(),
                "x-access-token",
                "G1/E2"
            );
            assert_eq!(
                material.fields["password"].expose_secret(),
                "github-token",
                "G1/E3"
            );
        }
        let input: ResourceInput = serde_json::from_value(json!({
            "type": "github_repository", "url": "https://github.com/acme/repo.git"
        }))
        .expect("G2 no token");
        assert!(
            matches!(
                input.to_input_request().target,
                awaken_session_application::SessionInputTarget::Repository {
                    credential_material: None,
                    ..
                }
            ),
            "G2 no invented material"
        );
    }
}
