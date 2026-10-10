// @formal entrypoint sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! Private-process transport for the existing RepositoryRealizer operation.
//! No Git policy lives here: clone, publication CAS and credential isolation
//! remain in git_transport. Requests/receipts are secret-free; credentials use
//! the existing materialized process environment, never serialized request data.

use crate::{IsolatedRoot, git_transport};
use awaken_provisioning_contract as pc;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) const MAX_FRAME: usize = 1024 * 1024;

#[derive(Clone, Copy, Serialize, Deserialize)]
enum CredentialKind {
    None,
    Upstream,
    Gateway,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    root: PathBuf,
    root_identity: (u64, u64),
    plan: pc::RepositoryRealizationPlan,
    publication: Option<pc::RepositoryPublicationExpectation>,
    credential: CredentialKind,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Outcome {
    Realized,
    Published(pc::RepositoryPublicationReceipt),
    Rejected(pc::RepositoryPublicationRejection),
    /// Wire v1: an untyped failure rendering. Still decoded so a helper binary
    /// from before typed failures cannot turn a failure into a protocol error;
    /// it carries no class, so the host can only treat it as a backend fault.
    Unavailable(String),
    /// Wire v2: the helper's failure with its class intact.
    Failed(Failure),
}

impl Outcome {
    /// Host-side projection: a failed helper exchange becomes the port error,
    /// every other outcome is returned for the caller's exact receipt match.
    pub(crate) fn into_result(self) -> Result<Self, pc::SandboxError> {
        match self {
            Self::Unavailable(legacy) => {
                Err(pc::SandboxError::backend("repository helper", legacy))
            }
            Self::Failed(failure) => Err(failure.into()),
            outcome @ (Self::Realized | Self::Published(_) | Self::Rejected(_)) => Ok(outcome),
        }
    }
}

/// Closed class vocabulary of the helper failure wire. Kept private to this
/// process pair so the neutral error type carries no serialization contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FailureClass {
    NotFound,
    Gone,
    NotReady,
    Transport,
    Timeout,
    Unauthorized,
    CapacityUnavailable,
    FenceRejected,
    Invalid,
    Unsupported,
    Conflict,
    Backend,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Failure {
    class: FailureClass,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    operation: Option<String>,
    detail: String,
}

impl From<pc::SandboxError> for Failure {
    fn from(error: pc::SandboxError) -> Self {
        use pc::SandboxError as E;
        let (class, operation) = match &error {
            E::NotFound(_) => (FailureClass::NotFound, None),
            E::Gone(_) => (FailureClass::Gone, None),
            E::NotReady(_) => (FailureClass::NotReady, None),
            E::Transport(_) => (FailureClass::Transport, None),
            E::Timeout(_) => (FailureClass::Timeout, None),
            E::Unauthorized(_) => (FailureClass::Unauthorized, None),
            E::CapacityUnavailable(_) => (FailureClass::CapacityUnavailable, None),
            E::FenceRejected(_) => (FailureClass::FenceRejected, None),
            E::Invalid(_) => (FailureClass::Invalid, None),
            E::Unsupported(_) => (FailureClass::Unsupported, None),
            E::Conflict(_) => (FailureClass::Conflict, None),
            E::Backend { operation, .. } => (FailureClass::Backend, Some((*operation).to_owned())),
        };
        Self {
            class,
            operation,
            detail: error.detail().to_owned(),
        }
    }
}

impl From<Failure> for pc::SandboxError {
    fn from(failure: Failure) -> Self {
        let Failure {
            class,
            operation,
            detail,
        } = failure;
        match class {
            FailureClass::NotFound => Self::NotFound(detail),
            FailureClass::Gone => Self::Gone(detail),
            FailureClass::NotReady => Self::NotReady(detail),
            FailureClass::Transport => Self::Transport(detail),
            FailureClass::Timeout => Self::Timeout(detail),
            FailureClass::Unauthorized => Self::Unauthorized(detail),
            FailureClass::CapacityUnavailable => Self::CapacityUnavailable(detail),
            FailureClass::FenceRejected => Self::FenceRejected(detail),
            FailureClass::Invalid => Self::Invalid(detail),
            FailureClass::Unsupported => Self::Unsupported(detail),
            FailureClass::Conflict => Self::Conflict(detail),
            // The operation label is process-static on each side; the remote
            // label stays visible in the detail instead of being forged.
            FailureClass::Backend => Self::backend(
                "repository helper",
                match operation {
                    Some(operation) => format!("{operation}: {detail}"),
                    None => detail,
                },
            ),
        }
    }
}

pub(crate) fn temporary_root(root: &Path) -> PathBuf {
    root.join(pc::WorkspaceLayout::AWAKEN_STATE_ROOT.trim_start_matches('/'))
        .join("repository-tmp")
}

impl Request {
    pub(crate) fn new(
        root: &Path,
        identity: awaken_sandbox_fs::DirectoryIdentity,
        plan: &pc::RepositoryRealizationPlan,
        publication: Option<&pc::RepositoryPublicationExpectation>,
        credential: Option<&pc::RepositoryHttpBasicCredential>,
    ) -> Self {
        Self {
            root: root.into(),
            root_identity: (identity.device, identity.inode),
            plan: plan.clone(),
            publication: publication.cloned(),
            credential: match credential {
                None => CredentialKind::None,
                Some(credential) if credential.is_gateway_capability() => CredentialKind::Gateway,
                Some(_) => CredentialKind::Upstream,
            },
        }
    }

    pub(crate) fn encode(&self) -> Result<Vec<u8>, pc::SandboxError> {
        self.plan.validate_mount_path()?;
        if let Some(expectation) = &self.publication {
            expectation.validate()?;
        }
        let bytes = serde_json::to_vec(self).map_err(|_| {
            pc::SandboxError::backend("encode", "repository request encoding failed")
        })?;
        if bytes.len() > MAX_FRAME {
            return Err(pc::SandboxError::invalid("repository request too large"));
        }
        Ok(bytes)
    }

    fn execute(&self) -> Result<Outcome, pc::SandboxError> {
        if !self.root.is_absolute() {
            return Err(pc::SandboxError::invalid(
                "repository root must be absolute",
            ));
        }
        let identity = awaken_sandbox_fs::directory_identity_nofollow(&self.root)
            .map_err(|_| pc::SandboxError::not_found("repository root is unavailable"))?;
        if (identity.device, identity.inode) != self.root_identity {
            return Err(pc::SandboxError::conflict(
                "repository root identity changed",
            ));
        }
        self.plan.validate_mount_path()?;
        let temporary = temporary_root(&self.root);
        if std::env::temp_dir() != temporary {
            return Err(pc::SandboxError::invalid(
                "repository temporary directory is not sandbox-owned",
            ));
        }
        awaken_sandbox_fs::create_relative_directory_all(
            &self.root,
            identity,
            temporary
                .strip_prefix(&self.root)
                .map_err(|_| pc::SandboxError::invalid("repository temporary directory escaped"))?,
        )
        .map_err(|_| {
            pc::SandboxError::backend("execute", "repository temporary directory unavailable")
        })?;
        let credential = self.credential.read_environment()?;
        let root = IsolatedRoot::new(&self.root);
        match &self.publication {
            None => git_transport::provision_repo_at(
                &root,
                &self.plan.mount_path,
                &self.plan.transport_url,
                self.plan.initial_branch.as_deref(),
                self.plan.initial_commit.as_deref(),
                credential.as_ref(),
            )
            .map(|_| Outcome::Realized),
            Some(expectation) => Ok(
                match git_transport::push_repo_to_at(
                    &root,
                    &self.plan.mount_path,
                    &self.plan,
                    expectation,
                    credential.as_ref(),
                ) {
                    Ok(receipt) => Outcome::Published(receipt),
                    Err(pc::RepositoryPublicationError::Rejected(rejection)) => {
                        Outcome::Rejected(rejection)
                    }
                    Err(pc::RepositoryPublicationError::Unavailable(error)) => {
                        Outcome::Failed(error.into())
                    }
                },
            ),
        }
    }
}

impl CredentialKind {
    fn read_environment(
        self,
    ) -> Result<Option<pc::RepositoryHttpBasicCredential>, pc::SandboxError> {
        if matches!(self, Self::None) {
            return Ok(None);
        }
        let read = |key| {
            std::env::var(key).map_err(|_| {
                pc::SandboxError::not_found("repository credential environment missing")
            })
        };
        let username = read(git_transport::GIT_CREDENTIAL_USERNAME_ENV)?;
        let password = read(git_transport::GIT_CREDENTIAL_PASSWORD_ENV)?;
        let material = pc::RepositoryHttpBasicCredential::new(username, password);
        Ok(Some(match self {
            Self::Gateway if material.expose_username() == "git" => {
                pc::RepositoryHttpBasicCredential::gateway_capability(
                    material.expose_password().to_owned(),
                )
            }
            Self::Upstream => material,
            _ => {
                return Err(pc::SandboxError::conflict(
                    "repository gateway credential shape changed",
                ));
            }
        }))
    }
}

pub(crate) fn environment(
    root: &Path,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Vec<pc::MaterializedEnvVar> {
    let value = |value: Option<&str>| {
        value.map_or_else(
            || pc::MaterializedEnvValue::Inline(String::new()),
            |value| pc::MaterializedEnvValue::Secret(value.to_owned().into()),
        )
    };
    vec![
        pc::MaterializedEnvVar {
            name: git_transport::GIT_CREDENTIAL_USERNAME_ENV.into(),
            value: value(credential.map(|c| c.expose_username())),
        },
        pc::MaterializedEnvVar {
            name: git_transport::GIT_CREDENTIAL_PASSWORD_ENV.into(),
            value: value(credential.map(|c| c.expose_password())),
        },
        pc::MaterializedEnvVar {
            name: "TMPDIR".into(),
            value: pc::MaterializedEnvValue::Inline(
                temporary_root(root).to_string_lossy().into_owned(),
            ),
        },
    ]
}

/// Internal execution-plane role. All command authority comes from its parent
/// ScopedProcess; this codec never authorizes a Session or publishes a receipt.
/// Parse/transport errors are deliberately payload-free.
pub fn serve(input: impl Read, mut output: impl Write) -> std::io::Result<()> {
    let mut bytes = Vec::new();
    input.take((MAX_FRAME + 1) as u64).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FRAME {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let request: Request =
        serde_json::from_slice(&bytes).map_err(|_| std::io::ErrorKind::InvalidData)?;
    let outcome = request
        .execute()
        .unwrap_or_else(|error| Outcome::Failed(error.into()));
    let bytes = serde_json::to_vec(&outcome).map_err(|_| std::io::ErrorKind::InvalidData)?;
    if bytes.len() > MAX_FRAME {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    output.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(root: &Path, credential: Option<&pc::RepositoryHttpBasicCredential>) -> Request {
        Request::new(
            root,
            awaken_sandbox_fs::directory_identity_nofollow(root).unwrap(),
            &pc::RepositoryRealizationPlan {
                repository_id: "repo".into(),
                mount_path: "/workspace/repo".into(),
                source_remote_url: "https://example.invalid/repo".into(),
                transport_url: "https://example.invalid/repo".into(),
                initial_branch: None,
                initial_commit: None,
                access: pc::MountAccess::ReadWrite,
            },
            None,
            credential,
        )
    }

    #[test]
    fn credentials_are_only_in_opaque_command_environment() {
        // C1 none/upstream/gateway x request/env -> R1 request contains kind,
        // never credential bytes; R2 supplied values are Secret; R3 absence
        // explicitly clears ambient credential keys. No process-global mutation.
        let root = tempfile::tempdir().unwrap();
        let upstream = pc::RepositoryHttpBasicCredential::new(
            "unique-repository-user".to_owned(),
            "unique-repository-password".to_owned(),
        );
        let gateway = pc::RepositoryHttpBasicCredential::gateway_capability(
            "unique-gateway-token".to_owned(),
        );
        for (credential, kind) in [
            (None, "None"),
            (Some(&upstream), "Upstream"),
            (Some(&gateway), "Gateway"),
        ] {
            let bytes = request(root.path(), credential).encode().unwrap();
            let encoded = String::from_utf8(bytes).unwrap();
            assert!(encoded.contains(kind));
            for secret in [
                "unique-repository-user",
                "unique-repository-password",
                "unique-gateway-token",
            ] {
                assert!(!encoded.contains(secret));
            }
            let env = environment(root.path(), credential);
            for variable in &env[..2] {
                if credential.is_some() {
                    assert!(matches!(
                        variable.value,
                        pc::MaterializedEnvValue::Secret(_)
                    ));
                    assert!(!format!("{variable:?}").contains(variable.value.expose()));
                } else {
                    assert_eq!(variable.value.expose(), "");
                }
            }
            assert_eq!(
                env[2].value.expose(),
                temporary_root(root.path()).to_str().unwrap()
            );
        }
    }

    #[test]
    fn request_admission_is_strict_bounded_and_payload_free() {
        // C2 parse/schema/size invalid -> R4 no execution or response bytes,
        // payload-free InvalidData. Valid shape + wrong exact identity -> R5
        // typed Unavailable without creating runtime temporary state.
        let root = tempfile::tempdir().unwrap();
        let valid = request(root.path(), None).encode().unwrap();
        let mut unknown: serde_json::Value = serde_json::from_slice(&valid).unwrap();
        unknown["unexpected"] = "synthetic-secret".into();
        let mut nested = unknown.clone();
        nested.as_object_mut().unwrap().remove("unexpected");
        nested["plan"]["unexpected"] = "synthetic-secret".into();
        for bytes in [
            b"synthetic-secret".to_vec(),
            vec![b'x'; MAX_FRAME + 1],
            serde_json::to_vec(&unknown).unwrap(),
            serde_json::to_vec(&nested).unwrap(),
        ] {
            let mut output = Vec::new();
            let error = serve(bytes.as_slice(), &mut output).unwrap_err();
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
            assert!(!error.to_string().contains("synthetic-secret"));
            assert!(output.is_empty());
        }
        let mut wrong = request(root.path(), None);
        wrong.root_identity.1 ^= 1;
        let mut output = Vec::new();
        serve(wrong.encode().unwrap().as_slice(), &mut output).unwrap();
        assert!(matches!(
            serde_json::from_slice::<Outcome>(&output)
                .unwrap()
                .into_result(),
            Err(pc::SandboxError::Conflict(_))
        ));
        assert!(!temporary_root(root.path()).exists());
    }

    #[test]
    fn helper_failure_wire_keeps_class_and_decodes_the_untyped_version() {
        // W1 every class round-trips through the helper wire with its detail;
        // W2 Backend keeps the remote operation visible under the host label;
        // W3 a v1 `Unavailable(String)` frame still decodes, as a backend fault;
        // W4 a success outcome passes through untouched.
        let classes = [
            pc::SandboxError::not_found("n"),
            pc::SandboxError::gone("g"),
            pc::SandboxError::not_ready("r"),
            pc::SandboxError::transport("t"),
            pc::SandboxError::timeout("o"),
            pc::SandboxError::unauthorized("u"),
            pc::SandboxError::capacity_unavailable("c"),
            pc::SandboxError::fence_rejected("f"),
            pc::SandboxError::invalid("i"),
            pc::SandboxError::unsupported("s"),
            pc::SandboxError::conflict("cannot push a repository with detached HEAD"),
        ];
        for error in classes {
            let bytes = serde_json::to_vec(&Outcome::Failed(error.clone().into())).unwrap();
            let decoded = serde_json::from_slice::<Outcome>(&bytes)
                .unwrap()
                .into_result();
            assert_eq!(decoded.err(), Some(error), "W1");
        }
        let bytes = serde_json::to_vec(&Outcome::Failed(
            pc::SandboxError::backend("git clone", "exit 128").into(),
        ))
        .unwrap();
        assert_eq!(
            serde_json::from_slice::<Outcome>(&bytes)
                .unwrap()
                .into_result()
                .err(),
            Some(pc::SandboxError::backend(
                "repository helper",
                "git clone: exit 128"
            )),
            "W2"
        );
        let legacy = br#"{"Unavailable":"sandbox error: repository root is unavailable"}"#;
        assert!(
            matches!(
                serde_json::from_slice::<Outcome>(legacy).unwrap().into_result(),
                Err(pc::SandboxError::Backend { operation: "repository helper", ref detail })
                    if detail == "sandbox error: repository root is unavailable"
            ),
            "W3"
        );
        let realized = serde_json::to_vec(&Outcome::Realized).unwrap();
        assert!(
            matches!(
                serde_json::from_slice::<Outcome>(&realized)
                    .unwrap()
                    .into_result(),
                Ok(Outcome::Realized)
            ),
            "W4"
        );
    }

    #[test]
    fn invalid_plan_or_expectation_is_rejected_before_launch() {
        // C3 mount/expectation/serialized size invalid -> R6 encode refuses
        // before any scope or Git effects. These checks reuse domain validators.
        let root = tempfile::tempdir().unwrap();
        let mut candidate = request(root.path(), None);
        candidate.plan.mount_path = "/workspace/.awaken/repo".into();
        assert!(candidate.encode().is_err());
        candidate.plan.mount_path = "/workspace/repo".into();
        candidate.publication = Some(pc::RepositoryPublicationExpectation {
            branch: "main".into(),
            commit: "invalid".into(),
            expected_prior_commit: None,
        });
        assert!(candidate.encode().is_err());
        candidate.publication = None;
        candidate.plan.repository_id = "x".repeat(MAX_FRAME);
        assert!(candidate.encode().is_err());
    }
}
