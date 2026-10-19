// @formal entrypoint credential_effect.repository_git_delivery_excludes_runtime_host_and_mixed_route_authority
//! Repository Git effects executed exclusively inside a container Sandbox.

use std::sync::Arc;
use std::time::Duration;

use awaken_provisioning_contract as pc;
use awaken_sandbox_control::{
    CapabilityExpiresAtUnixMs, REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH, RepositoryGitCredentialQuery,
    SandboxControlRequest, SandboxControlResponse, SandboxControlSecret, SandboxControlService,
    SandboxControlServiceKind,
};
use tokio::io::AsyncReadExt;

const CREDENTIAL_OPERATION_TTL: Duration = Duration::from_secs(120);

fn repository_git_tls_env(
    remote_url: &str,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Vec<pc::EnvVar> {
    let private_gateway = credential
        .is_some_and(pc::RepositoryHttpBasicCredential::is_gateway_capability)
        && url::Url::parse(remote_url)
            .ok()
            .is_some_and(|url| url.host_str().is_some_and(|host| host.ends_with(".svc")));
    if !private_gateway {
        return Vec::new();
    }
    vec![pc::EnvVar {
        name: "GIT_SSL_CAINFO".into(),
        value: pc::EnvValue::Inline {
            value: awaken_container_contract::REPOSITORY_GATEWAY_CA_PATH.into(),
        },
        visibility: pc::EnvVisibility::Process,
    }]
}

struct OperationRepositoryCredential {
    protocol: String,
    host: String,
    path: String,
    username: String,
    password: String,
    expires_at_unix_ms: u64,
    /// The Host's process clock the expiry is stamped and judged on.
    clock: Arc<dyn awaken_time::WallClock>,
}

impl OperationRepositoryCredential {
    fn new(
        remote_url: &str,
        credential: &pc::RepositoryHttpBasicCredential,
        clock: Arc<dyn awaken_time::WallClock>,
    ) -> Result<Self, pc::SandboxError> {
        let url = url::Url::parse(remote_url)
            .map_err(|_| pc::SandboxError::invalid("Repository transport URL is invalid"))?;
        if url.scheme() != "https" || url.query().is_some() || url.fragment().is_some() {
            return Err(pc::SandboxError::invalid(
                "credentialed Repository transport must use a canonical HTTPS URL",
            ));
        }
        let mut host = url
            .host_str()
            .ok_or_else(|| pc::SandboxError::invalid("Repository transport URL has no host"))?
            .to_owned();
        if let Some(port) = url.port() {
            host.push(':');
            host.push_str(&port.to_string());
        }
        let path = url.path().trim_start_matches('/').to_owned();
        if path.is_empty() {
            return Err(pc::SandboxError::invalid(
                "Repository transport URL has no path",
            ));
        }
        let ttl_ms = u64::try_from(CREDENTIAL_OPERATION_TTL.as_millis()).map_err(|_| {
            pc::SandboxError::backend("new", "Repository credential expiry overflow")
        })?;
        let expires_at_unix_ms = awaken_time::WallClockExt::now_ms(clock.as_ref())
            .checked_add(ttl_ms)
            .ok_or_else(|| {
                pc::SandboxError::backend("new", "Repository credential expiry overflow")
            })?;
        Ok(Self {
            protocol: url.scheme().to_owned(),
            host,
            path,
            username: credential.expose_username().to_owned(),
            password: credential.expose_password().to_owned(),
            expires_at_unix_ms,
            clock,
        })
    }

    fn matches(&self, query: &RepositoryGitCredentialQuery) -> bool {
        query.protocol == self.protocol
            && query.host == self.host
            && query.path.trim_start_matches('/') == self.path
    }
}

#[async_trait::async_trait]
impl SandboxControlService for OperationRepositoryCredential {
    async fn handle(&self, request: SandboxControlRequest) -> SandboxControlResponse {
        let SandboxControlRequest::RepositoryGitCredentialGet { query } = request;
        let now = awaken_time::WallClockExt::now_ms(self.clock.as_ref());
        if !self.matches(&query) || now >= self.expires_at_unix_ms {
            return SandboxControlResponse::Denied;
        }
        let Ok(username) = SandboxControlSecret::new(self.username.clone()) else {
            return SandboxControlResponse::Unavailable;
        };
        let Ok(password) = SandboxControlSecret::new(self.password.clone()) else {
            return SandboxControlResponse::Unavailable;
        };
        SandboxControlResponse::RepositoryGitCredential {
            username,
            password,
            expires_at_unix_ms: CapabilityExpiresAtUnixMs(self.expires_at_unix_ms),
        }
    }
}

async fn publish_credential(
    sandbox: &dyn awaken_provisioning_contract::SessionSandbox,
    remote_url: &str,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
    clock: &Arc<dyn awaken_time::WallClock>,
) -> Result<Option<Box<dyn awaken_sandbox_control::PublishedSandboxControlService>>, pc::SandboxError>
{
    let Some(credential) = credential else {
        return Ok(None);
    };
    let service = Arc::new(OperationRepositoryCredential::new(
        remote_url,
        credential,
        clock.clone(),
    )?);
    sandbox
        .publish_sandbox_control_service(
            SandboxControlServiceKind::RepositoryGitCredential,
            service,
        )
        .await
        .map(Some)
        .map_err(|_| {
            pc::SandboxError::transport("Sandbox Repository credential channel is unavailable")
        })
}

pub(super) async fn provision(
    sandbox: &dyn awaken_provisioning_contract::SessionSandbox,
    publisher_bin: &str,
    plan: &pc::RepositoryRealizationPlan,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
    clock: &Arc<dyn awaken_time::WallClock>,
    intent: pc::RepositoryRealizationIntent,
) -> Result<(), pc::SandboxError> {
    plan.validate_mount_path()?;
    intent.validate_owned_path(plan, sandbox.handle().owned_paths())?;
    let publication = if intent == pc::RepositoryRealizationIntent::Initialize {
        publish_credential(sandbox, &plan.transport_url, credential, clock).await?
    } else {
        None
    };
    let mut command = repository_clone_command(plan, publisher_bin, uuid::Uuid::new_v4(), intent)?;
    command
        .env
        .extend(repository_git_tls_env(&plan.transport_url, credential));
    let result = run_repository_command(sandbox, command, 16 * 1024).await;
    if let Some(publication) = publication {
        publication.close().await;
    }
    let (status, observation) = result?;
    if status.code == Some(0) {
        sandbox.record_owned_path(&plan.mount_path)?;
        Ok(())
    } else {
        let observation = clone_failure_diagnosis(observation);
        Err(pc::SandboxError::backend(
            "provision",
            format!(
                "container Repository clone exited {:?}; diagnosis={observation}",
                status.code
            ),
        ))
    }
}

// Drain the process channel through EOF: truncating the reader can block Git
// on a full pipe. Keep only a bounded tail in memory, never persist raw output,
// and return a closed vocabulary that cannot contain a URL or credential.
struct RepositoryObservation {
    tail: Vec<u8>,
    truncated: bool,
}

async fn run_repository_command(
    sandbox: &dyn awaken_provisioning_contract::SessionSandbox,
    command: pc::Command,
    limit: usize,
) -> Result<(pc::ExitStatus, Result<RepositoryObservation, ()>), pc::SandboxError> {
    let mut process = sandbox.spawn_agent_process(command).await?;
    let (observation, status) = tokio::join!(
        read_repository_observation(&mut process.channel, limit),
        process.process.wait()
    );
    status.map(|status| (status, observation))
}

async fn read_repository_observation(
    reader: &mut (impl tokio::io::AsyncRead + Unpin),
    limit: usize,
) -> Result<RepositoryObservation, ()> {
    let mut tail = Vec::with_capacity(limit);
    let mut truncated = false;
    let mut chunk = [0; 4096];
    loop {
        match reader.read(&mut chunk).await {
            Ok(0) => break,
            Ok(size) => {
                let keep = size.min(limit);
                let remove = (tail.len() + keep).saturating_sub(limit);
                truncated |= remove != 0 || size > limit;
                tail.drain(..remove);
                tail.extend_from_slice(&chunk[size - keep..size]);
            }
            Err(_) => return Err(()),
        }
    }
    Ok(RepositoryObservation { tail, truncated })
}

fn clone_failure_diagnosis(observation: Result<RepositoryObservation, ()>) -> &'static str {
    let Ok(observation) = observation else {
        return "observation_unavailable";
    };
    let text = String::from_utf8_lossy(&observation.tail).to_ascii_lowercase();
    for (fragment, diagnosis) in [
        ("requested url returned error: 401", "http_401"),
        ("requested url returned error: 403", "http_403"),
        ("requested url returned error: 404", "http_404"),
        ("requested url returned error: 429", "http_429"),
        ("requested url returned error: 500", "http_500"),
        ("requested url returned error: 502", "http_502"),
        ("requested url returned error: 503", "http_503"),
        ("requested url returned error: 504", "http_504"),
    ] {
        if text.contains(fragment) {
            return diagnosis;
        }
    }
    for (diagnosis, fragments) in [
        (
            "dns",
            &["could not resolve host", "could not resolve proxy"][..],
        ),
        (
            "tls",
            &[
                "ssl certificate problem",
                "certificate verify failed",
                "server certificate verification failed",
            ][..],
        ),
        (
            "authentication",
            &["authentication failed", "could not read username"][..],
        ),
        ("remote_missing", &["repository not found"][..]),
        (
            "transport",
            &[
                "connection reset",
                "failed to connect",
                "connection timed out",
                "early eof",
                "curl 92",
            ][..],
        ),
        (
            "disk",
            &["no space left on device", "disk quota exceeded"][..],
        ),
        (
            "checkout",
            &["pathspec", "reference is not a tree", "invalid reference"][..],
        ),
        (
            "destination_conflict",
            &[
                "repository destination is occupied",
                "repository destination became occupied",
            ][..],
        ),
        (
            "retained_missing",
            &["retained repository working tree is absent"][..],
        ),
    ] {
        if fragments.iter().any(|fragment| text.contains(fragment)) {
            return diagnosis;
        }
    }
    "unclassified"
}

fn repository_clone_command(
    plan: &pc::RepositoryRealizationPlan,
    publisher_bin: &str,
    stage_identity: uuid::Uuid,
    intent: pc::RepositoryRealizationIntent,
) -> Result<pc::Command, pc::SandboxError> {
    let parent = plan
        .mount_path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .ok_or_else(|| pc::SandboxError::invalid("Repository destination has no sandbox parent"))?;
    let stage = format!(
        "{parent}/.awaken-repository-{}.stage",
        stage_identity.simple()
    );
    Ok(pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "set -eu; exec 2>&1; export GIT_TERMINAL_PROMPT=0 LC_ALL=C; destination=$1; parent=$2; stage=$3; remote=$4; branch=$5; commit=$6; publisher=$7; helper=$8; socket=$9; intent=${10}; ",
                "credential_helper=\"!$helper git-credential --socket $socket\"; stage_owned=0; ",
                "cleanup() { if test \"$stage_owned\" = 1; then rm -rf -- \"$stage\"; fi; }; trap cleanup EXIT; ",
                "is_occupied() { test -e \"$1\" || test -L \"$1\"; }; ",
                "is_exact() { candidate=$1; test -d \"$candidate\" && test ! -L \"$candidate\" || return 1; ",
                "test -d \"$candidate/.git\" && test ! -L \"$candidate/.git\" || return 1; ",
                "test \"$(git -C \"$candidate\" remote get-url origin)\" = \"$remote\" || return 1; ",
                "git -C \"$candidate\" rev-parse --verify 'HEAD^{commit}' >/dev/null 2>&1 || return 1; ",
                "if test -n \"$branch\"; then test \"$(git -C \"$candidate\" symbolic-ref --short HEAD)\" = \"$branch\" || return 1; fi; ",
                "if test -n \"$commit\"; then test \"$(git -C \"$candidate\" rev-parse HEAD)\" = \"$(git -C \"$candidate\" rev-parse \"$commit\")\" || return 1; fi; }; ",
                "if is_occupied \"$destination\"; then is_exact \"$destination\" && exit 0; echo 'Repository destination is occupied by a non-exact realization' >&2; exit 1; fi; ",
                "if test \"$intent\" = retain; then echo 'Retained Repository working tree is absent' >&2; exit 1; fi; ",
                "mkdir -p -- \"$parent\"; mkdir -m 700 -- \"$stage\"; stage_owned=1; ",
                "git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" clone --no-checkout -- \"$remote\" \"$stage\"; ",
                "if test -n \"$branch\"; then git -C \"$stage\" checkout \"$branch\"; fi; ",
                "if test -n \"$commit\"; then git -C \"$stage\" checkout --detach \"$commit\"; else git -C \"$stage\" reset --hard HEAD; fi; is_exact \"$stage\"; ",
                "if is_occupied \"$destination\"; then is_exact \"$destination\" && exit 0; echo 'Repository destination became occupied before publication' >&2; exit 1; fi; ",
                "publish_status=0; \"$publisher\" repository-publish-noreplace \"$stage\" \"$destination\" || publish_status=$?; ",
                "if test \"$publish_status\" != 0; then is_exact \"$destination\" && exit 0; exit \"$publish_status\"; fi; stage_owned=0; is_exact \"$destination\""
            )
            .into(),
            "awaken-repo-clone".into(),
            plan.mount_path.clone(),
            parent.into(),
            stage,
            plan.transport_url.clone(),
            intent.checkout_constraints(plan).0.unwrap_or_default().to_owned(),
            intent.checkout_constraints(plan).1.unwrap_or_default().to_owned(),
            publisher_bin.into(),
            publisher_bin.into(),
            REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH.into(),
            if intent == pc::RepositoryRealizationIntent::Retain { "retain" } else { "initialize" }.into(),
        ],
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Piped,
    })
}

pub(super) async fn push(
    sandbox: &dyn awaken_provisioning_contract::SessionSandbox,
    helper_bin: &str,
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
    clock: &Arc<dyn awaken_time::WallClock>,
) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
    plan.validate_mount_path()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    expectation
        .validate()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    if plan.access == pc::MountAccess::ReadOnly {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::unauthorized("read-only repository cannot be published"),
        ));
    }
    let publication = publish_credential(sandbox, &plan.transport_url, credential, clock)
        .await
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    let mut command = repository_push_command(plan, expectation, helper_bin);
    command
        .env
        .extend(repository_git_tls_env(&plan.transport_url, credential));
    let result = run_repository_command(sandbox, command, 64).await;
    if let Some(publication) = publication {
        publication.close().await;
    }
    let (status, observation) = result.map_err(pc::RepositoryPublicationError::Unavailable)?;
    let observation = observation
        .ok()
        .filter(|observation| !observation.truncated)
        .and_then(|observation| String::from_utf8(observation.tail).ok())
        .ok_or_else(|| {
            pc::RepositoryPublicationError::Unavailable(pc::SandboxError::backend(
                "push-observation",
                "Repository observation is unavailable or malformed",
            ))
        })?;
    match status.code {
        Some(0) if observation.is_empty() => {
            Ok(pc::RepositoryPublicationReceipt::new(plan, expectation))
        }
        Some(0) => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::invalid("container Repository success emitted unexpected output"),
        )),
        Some(42) => match decode_publication_rejection(&observation, expectation) {
            Ok(rejection) => Err(pc::RepositoryPublicationError::Rejected(rejection)),
            Err(error) => Err(pc::RepositoryPublicationError::Unavailable(error)),
        },
        Some(43) => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::backend(
                "repository-publication",
                "container Repository observation or push remained unavailable",
            ),
        )),
        code => Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::backend("push", format!("container Repository push exited {code:?}")),
        )),
    }
}

fn decode_publication_rejection(
    observation: &str,
    expectation: &pc::RepositoryPublicationExpectation,
) -> Result<pc::RepositoryPublicationRejection, pc::SandboxError> {
    let rejection = match observation {
        "ABSENT\n" => pc::RepositoryPublicationRejection::RemoteRefAbsent,
        value if value.starts_with("LOCAL:") => {
            pc::RepositoryPublicationRejection::LocalCommitMismatch {
                observed_commit: value
                    .strip_prefix("LOCAL:")
                    .and_then(|value| value.strip_suffix('\n'))
                    .ok_or_else(|| {
                        pc::SandboxError::invalid(
                            "container Repository local mismatch has no exact HEAD observation",
                        )
                    })?
                    .to_owned(),
            }
        }
        value => pc::RepositoryPublicationRejection::RemoteRefChanged {
            observed_commit: value
                .strip_prefix("REF:")
                .and_then(|value| value.strip_suffix('\n'))
                .ok_or_else(|| {
                    pc::SandboxError::invalid(
                        "container Repository conflict has no exact remote observation",
                    )
                })?
                .to_owned(),
        },
    };
    rejection.verify(expectation)?;
    Ok(rejection)
}

fn repository_push_command(
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    helper_bin: &str,
) -> pc::Command {
    pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "set -eu; export GIT_TERMINAL_PROMPT=0; repo=$1; remote=$2; branch=$3; commit=$4; prior=$5; helper=$6; socket=$7; ",
                "credential_helper=\"!$helper git-credential --socket $socket\"; ",
                "git check-ref-format --branch \"$branch\" >/dev/null 2>&1 || exit 43; ",
                "test \"$(git -C \"$repo\" symbolic-ref --quiet --short HEAD)\" = \"$branch\" || exit 43; ",
                "local_commit=$(git -C \"$repo\" rev-parse --verify 'HEAD^{commit}' 2>/dev/null) || exit 43; ",
                "test \"${#local_commit}\" -eq 40 || exit 43; ",
                "case \"$local_commit\" in *[!0123456789abcdef]*) exit 43 ;; esac; ",
                "if test \"$local_commit\" != \"$commit\"; then printf 'LOCAL:%s\\n' \"$local_commit\"; exit 42; fi; ",
                "observe_remote() { remote_refs=; for attempt in 1 2 3; do ",
                "if remote_refs=$(git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" ls-remote --refs \"$remote\" \"refs/heads/$branch\" 2>/dev/null); then break; fi; ",
                "if test \"$attempt\" = 3; then return 1; fi; sleep 1; done; ",
                "test \"$(printf '%s\\n' \"$remote_refs\" | awk 'END { print NR }')\" -eq 1 || return 1; ",
                "remote_commit=$(printf '%s\\n' \"$remote_refs\" | awk -v expected=\"refs/heads/$branch\" 'NF == 2 && $2 == expected { print $1 }'); ",
                "if test -n \"$remote_refs\" && test -z \"$remote_commit\"; then return 1; fi; }; ",
                "reject_stale() { if test -z \"$remote_commit\"; then printf 'ABSENT\\n'; exit 42; fi; ",
                "test \"${#remote_commit}\" -eq 40 || exit 43; ",
                "case \"$remote_commit\" in *[!0123456789abcdef]*) exit 43 ;; esac; ",
                "printf 'REF:%s\\n' \"$remote_commit\"; exit 42; }; ",
                "observe_remote || exit 43; ",
                "if test \"$remote_commit\" = \"$commit\"; then exit 0; fi; ",
                "if test \"$remote_commit\" != \"$prior\"; then reject_stale; fi; ",
                "before=$remote_commit; ",
                "if git -c credential.useHttpPath=true -c \"credential.helper=$credential_helper\" -C \"$repo\" push --porcelain --force-with-lease=\"refs/heads/$branch:$prior\" \"$remote\" \"$commit:refs/heads/$branch\" >/dev/null 2>&1; then exit 0; fi; ",
                "observe_remote || exit 43; ",
                "if test \"$remote_commit\" = \"$commit\"; then exit 0; fi; ",
                "if test \"$remote_commit\" != \"$before\"; then reject_stale; fi; ",
                "exit 43"
            )
            .into(),
            "awaken-repo-push".into(),
            plan.mount_path.clone(),
            plan.transport_url.clone(),
            expectation.branch.clone(),
            expectation.commit.clone(),
            expectation.expected_prior_commit.clone().unwrap_or_default(),
            helper_bin.into(),
            REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH.into(),
        ],
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Piped,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn clone_failure_observation(
        reader: &mut (impl tokio::io::AsyncRead + Unpin),
    ) -> &'static str {
        clone_failure_diagnosis(read_repository_observation(reader, 16 * 1024).await)
    }

    #[test]
    fn only_private_gateway_git_operations_select_the_internal_ca() {
        // Causes: C1=Gateway capability vs upstream/no credential; C2=private
        // Service DNS vs public host. Effects: E1=the single Git operation
        // receives the public internal CA; E2=all other Git processes retain
        // their normal system trust. Rules: T1(gateway,svc)->E1;
        // T2(gateway,public|upstream,any|absent,any)->E2.
        let gateway =
            pc::RepositoryHttpBasicCredential::gateway_capability("signed-capability".to_owned());
        let upstream =
            pc::RepositoryHttpBasicCredential::new("user".to_owned(), "password".to_owned());
        let private =
            repository_git_tls_env("https://gateway.awaken.svc:8444/git/repo", Some(&gateway));
        assert_eq!(private.len(), 1, "T1");
        assert_eq!(private[0].name, "GIT_SSL_CAINFO", "T1");
        assert_eq!(
            private[0].value,
            pc::EnvValue::Inline {
                value: awaken_container_contract::REPOSITORY_GATEWAY_CA_PATH.into()
            },
            "T1"
        );
        for (url, credential) in [
            ("https://gateway.example.com/git/repo", Some(&gateway)),
            ("https://gateway.awaken.svc:8444/git/repo", Some(&upstream)),
            ("https://gateway.awaken.svc:8444/git/repo", None),
        ] {
            assert!(
                repository_git_tls_env(url, credential).is_empty(),
                "T2 {url}"
            );
        }
    }

    fn plan() -> pc::RepositoryRealizationPlan {
        pc::RepositoryRealizationPlan {
            repository_id: "repository".into(),
            mount_path: "/workspace/source/repository".into(),
            source_remote_url: "https://example.invalid/repository".into(),
            transport_url: "https://example.invalid/repository".into(),
            initial_branch: Some("main".into()),
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        }
    }

    #[tokio::test]
    async fn failed_clone_reports_safe_diagnosis_without_publishing_raw_output() {
        // C1=nonzero clone exit; C2=Git DNS failure containing a credential;
        // E1=retain the exit status and diagnose DNS; E2=no raw output or secret
        // escapes the Repository boundary. R1(C1,C2)->E1+E2, no success claim.
        use crate::test_support::{FakeContainerProvider, spec};
        use awaken_provisioning_contract::SandboxProvider as _;
        let shared = Arc::new(std::sync::Mutex::new(std::collections::HashMap::from([
            ("__fail_repository_import".into(), Vec::new()),
            ("__repository_import_observation".into(), b"fatal: unable to access 'https://user:secret-fixture@example.invalid/': Could not resolve host: example.invalid\n".to_vec()),
        ])));
        let provider = FakeContainerProvider {
            shared: shared.clone(),
            ..Default::default()
        };
        let container = provider
            .claim_for_effect(
                &spec(),
                awaken_provisioning_contract::SandboxEffect::Unattached,
                awaken_provisioning_contract::SandboxRealizationIntent::Create,
            )
            .await
            .unwrap();
        let error = provision(
            container.as_ref(),
            "publisher",
            &plan(),
            None,
            &awaken_time::system_clock(),
            pc::RepositoryRealizationIntent::Initialize,
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("clone exited Some(23)"));
        assert!(error.contains("diagnosis=dns"), "{error}");
        assert!(!error.contains("secret-fixture"));
        assert!(!error.contains("example.invalid"));
        // R2 success with misleading diagnostic text must remain success:
        // diagnostics cannot override the observed process outcome.
        shared.lock().unwrap().remove("__fail_repository_import");
        provision(
            container.as_ref(),
            "publisher",
            &plan(),
            None,
            &awaken_time::system_clock(),
            pc::RepositoryRealizationIntent::Initialize,
        )
        .await
        .expect("R2 exit zero remains authoritative");
    }

    #[tokio::test]
    async fn oversized_or_invalid_push_observation_cannot_publish_rejection() {
        // C1=exit 42; C2=oversized output ending in valid ABSENT vs invalid
        // UTF8 vs exact ABSENT. R1 oversized/invalid->Unavailable, never infer
        // an exact remote observation from a truncated tail. R2 exact->Rejected.
        // A MiB exceeds both the output bound and pipe capacity: it must drain.
        use crate::test_support::{FakeContainerProvider, spec};
        use awaken_provisioning_contract::SandboxProvider as _;
        let provider = FakeContainerProvider::default();
        let container = provider
            .claim_for_effect(
                &spec(),
                awaken_provisioning_contract::SandboxEffect::Unattached,
                awaken_provisioning_contract::SandboxRealizationIntent::Create,
            )
            .await
            .unwrap();
        let expectation = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: "1".repeat(40),
            expected_prior_commit: Some("2".repeat(40)),
        };
        let mut large = vec![b'x'; 1024 * 1024];
        large.extend_from_slice(b"ABSENT\n");
        for (wire, unavailable) in [
            (large, true),
            (vec![0xff], true),
            (b"ABSENT\n".to_vec(), false),
        ] {
            provider
                .shared
                .lock()
                .unwrap()
                .insert("__repository_export_observation".into(), wire);
            let error = tokio::time::timeout(
                Duration::from_secs(5),
                push(
                    container.as_ref(),
                    "publisher",
                    &plan(),
                    &expectation,
                    None,
                    &awaken_time::system_clock(),
                ),
            )
            .await
            .unwrap()
            .unwrap_err();
            assert_eq!(
                matches!(error, pc::RepositoryPublicationError::Unavailable(_)),
                unavailable
            );
        }
    }

    #[tokio::test]
    async fn clone_diagnosis_drains_large_output_and_only_returns_closed_categories() {
        // C1=recognized/unknown stderr; C2=output exceeds the retained bound;
        // C3=read failure. R1 recognized->fixed category; R2 unknown->unclassified;
        // R3 large->drain through EOF, classify tail; R4 read error->unavailable.
        // None permits raw text, secret disclosure, replay, or success inference.
        for (raw, expected) in [
            ("Could not resolve proxy: secret-fixture", "dns"),
            ("SSL certificate problem: secret-fixture", "tls"),
            (
                "fatal: Authentication failed for secret-fixture",
                "authentication",
            ),
            (
                "The requested URL returned error: 403 secret-fixture",
                "http_403",
            ),
            (
                "fatal: repository not found secret-fixture",
                "remote_missing",
            ),
            (
                "The requested URL returned error: 503 secret-fixture",
                "http_503",
            ),
            ("fatal: early EOF secret-fixture", "transport"),
            ("No space left on device secret-fixture", "disk"),
            ("error: pathspec secret-fixture did not match", "checkout"),
            (
                "Repository destination became occupied before publication",
                "destination_conflict",
            ),
            (
                "Retained Repository working tree is absent",
                "retained_missing",
            ),
            ("unknown secret-fixture", "unclassified"),
            ("", "unclassified"),
        ] {
            let mut reader = raw.as_bytes();
            assert_eq!(clone_failure_observation(&mut reader).await, expected);
        }
        let (mut reader, mut writer) = tokio::io::duplex(1024);
        // R1 also retains the exact allowlisted HTTP status for the owner to
        // distinguish authorization, absence, throttling, and upstream failures.
        for code in [401, 403, 404, 429, 500, 502, 503, 504] {
            let raw = format!("fatal: The requested URL returned error: {code} secret-fixture");
            assert_eq!(
                clone_failure_observation(&mut raw.as_bytes()).await,
                format!("http_{code}")
            );
        }
        let producer = tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            writer.write_all(&vec![b'x'; 1024 * 1024]).await.unwrap();
            writer
                .write_all(b"fatal: Connection reset by peer secret-fixture")
                .await
                .unwrap();
        });
        assert_eq!(
            tokio::time::timeout(
                Duration::from_secs(5),
                clone_failure_observation(&mut reader)
            )
            .await
            .unwrap(),
            "transport",
            "R3 must drain beyond the pipe capacity"
        );
        producer.await.unwrap();
        struct BrokenReader;
        impl tokio::io::AsyncRead for BrokenReader {
            fn poll_read(
                self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
                _: &mut tokio::io::ReadBuf<'_>,
            ) -> std::task::Poll<std::io::Result<()>> {
                std::task::Poll::Ready(Err(std::io::Error::other("secret-fixture")))
            }
        }
        assert_eq!(
            clone_failure_observation(&mut BrokenReader).await,
            "observation_unavailable"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clone_program_exposes_git_failure_and_removes_only_owned_staging() {
        // R1 Initialize + known failed Git exit: preserve exit 128, forward
        // stderr through the existing channel, remove owned staging, leave
        // destination absent. Execute the emitted program with only Git replaced.
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let git = temp.path().join("git");
        std::fs::write(&git, "#!/bin/sh\necho 'fatal: The requested URL returned error: 503 secret-fixture' >&2\nexit 128\n").unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = repository_clone_command(
            &plan(),
            "unused-publisher",
            uuid::Uuid::nil(),
            pc::RepositoryRealizationIntent::Initialize,
        )
        .unwrap();
        let destination = temp.path().join("destination");
        let stage = temp.path().join("stage");
        command.argv[4] = destination.to_string_lossy().into_owned();
        command.argv[5] = temp.path().to_string_lossy().into_owned();
        command.argv[6] = stage.to_string_lossy().into_owned();
        let output = std::process::Command::new(&command.argv[0])
            .args(&command.argv[1..])
            .env("PATH", format!("{}:/usr/bin:/bin", temp.path().display()))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(128));
        assert!(
            output.stderr.is_empty(),
            "Git stderr must reach stdout channel"
        );
        assert_eq!(
            clone_failure_observation(&mut output.stdout.as_slice()).await,
            "http_503"
        );
        assert!(!stage.exists());
        assert!(!destination.exists());
    }

    #[test]
    fn clone_command_executes_network_git_in_sandbox_and_never_embeds_credentials() {
        let command = repository_clone_command(
            &plan(),
            "/opt/custom/awaken-sandbox",
            uuid::Uuid::nil(),
            pc::RepositoryRealizationIntent::Initialize,
        )
        .expect("valid clone command");
        let script = &command.argv[2];
        assert!(script.contains("clone --no-checkout -- \"$remote\" \"$stage\""));
        assert!(script.contains("credential.helper=$credential_helper"));
        assert_eq!(command.argv[10], "/opt/custom/awaken-sandbox");
        assert_eq!(command.argv[11], "/opt/custom/awaken-sandbox");
        assert_eq!(command.argv[12], REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH);
        assert!(!format!("{command:?}").contains("password"));
        assert!(!script.contains("rmdir -- \"$stage\""));
        assert_eq!(script.matches("rm -rf --").count(), 1);
        assert!(!script.contains("rm -rf -- \"$destination\""));
        assert!(script.contains("repository-publish-noreplace"));
    }

    #[cfg(unix)]
    #[test]
    fn retained_container_git_program_preserves_wip_and_refuses_invalid_continuity() {
        // Production-program matrix: C1 initialize/retain; C2 writable/readonly;
        // C3 exact/foreign origin; C4 directory/missing/symlink. R1 initialized
        // writable retention permits changed branch/HEAD/WIP; R2 Initialize or
        // readonly keeps checkout constraints; R3 foreign/missing/symlink
        // refuses with no clone, reset, staging directory or content loss.
        // Only coordinates point at an isolated fixture; execute the actual
        // emitted container program, never a copy of its decision logic.
        let temporary = tempfile::tempdir().unwrap();
        let repo = temporary.path().join("repository");
        let git = |args: &[&str]| {
            let output = std::process::Command::new("git")
                .args(args)
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        };
        git(&["init", "--initial-branch=main", repo.to_str().unwrap()]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.name",
            "fixture",
        ]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "config",
            "user.email",
            "fixture@example.invalid",
        ]);
        std::fs::write(repo.join("README"), "checkpoint").unwrap();
        git(&["-C", repo.to_str().unwrap(), "add", "README"]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-m",
            "checkpoint",
        ]);
        git(&[
            "-C",
            repo.to_str().unwrap(),
            "remote",
            "add",
            "origin",
            &plan().transport_url,
        ]);
        git(&["-C", repo.to_str().unwrap(), "checkout", "-b", "awf/work"]);
        std::fs::write(repo.join("README"), "dirty").unwrap();
        std::fs::write(repo.join("new.txt"), "untracked").unwrap();
        let execute = |plan: pc::RepositoryRealizationPlan, intent| {
            let mut command =
                repository_clone_command(&plan, "unused-helper", uuid::Uuid::nil(), intent)
                    .unwrap();
            command.argv[4] = repo.to_string_lossy().into_owned();
            command.argv[5] = temporary.path().to_string_lossy().into_owned();
            command.argv[6] = temporary
                .path()
                .join("must-not-create-stage")
                .to_string_lossy()
                .into_owned();
            let output = std::process::Command::new(&command.argv[0])
                .args(&command.argv[1..])
                .output()
                .unwrap();
            assert!(!temporary.path().join("must-not-create-stage").exists());
            output.status.success()
        };
        assert!(
            execute(plan(), pc::RepositoryRealizationIntent::Retain),
            "R1"
        );
        assert!(
            !execute(plan(), pc::RepositoryRealizationIntent::Initialize),
            "R2 initial ref remains strict"
        );
        let mut readonly = plan();
        readonly.access = pc::MountAccess::ReadOnly;
        assert!(
            !execute(readonly, pc::RepositoryRealizationIntent::Retain),
            "R2 readonly"
        );
        let mut foreign = plan();
        foreign.transport_url = "https://foreign.example.invalid/repository".into();
        assert!(
            !execute(foreign, pc::RepositoryRealizationIntent::Retain),
            "R3 foreign"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("README")).unwrap(),
            "dirty"
        );
        assert_eq!(
            std::fs::read_to_string(repo.join("new.txt")).unwrap(),
            "untracked"
        );
        let backup = temporary.path().join("backup");
        std::fs::rename(&repo, &backup).unwrap();
        assert!(
            !execute(plan(), pc::RepositoryRealizationIntent::Retain),
            "R3 missing"
        );
        assert!(!repo.exists(), "no hidden clone");
        std::os::unix::fs::symlink(&backup, &repo).unwrap();
        assert!(
            !execute(plan(), pc::RepositoryRealizationIntent::Retain),
            "R3 symlink"
        );
        assert_eq!(
            std::fs::read_to_string(backup.join("README")).unwrap(),
            "dirty"
        );
    }

    #[test]
    fn push_command_is_exact_cas_and_executes_only_in_sandbox() {
        let expectation = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: "0123456789012345678901234567890123456789".into(),
            expected_prior_commit: Some("1111111111111111111111111111111111111111".into()),
        };
        let command = repository_push_command(&plan(), &expectation, "/opt/custom/awaken-sandbox");
        let script = &command.argv[2];
        assert!(script.contains("git check-ref-format --branch"));
        assert!(script.contains("ls-remote --refs"));
        assert!(script.contains("if remote_refs=$(git"));
        assert!(script.contains("exit 43"));
        assert!(script.contains("--force-with-lease="));
        assert!(script.contains("credential.helper=$credential_helper"));
        assert!(script.contains("exit 42"));
        assert_eq!(command.stdio, pc::Stdio::Piped);
        assert!(script.contains("printf 'REF:%s"));
        assert!(script.contains("printf 'LOCAL:%s"));
        assert!(
            script.find("printf 'LOCAL:%s").unwrap() < script.find("ls-remote").unwrap(),
            "local mismatch must be proven before the first remote operation"
        );
        assert!(script.contains("push --porcelain"));
        assert!(script.contains(">/dev/null 2>&1"));
        assert_eq!(command.argv[9], "/opt/custom/awaken-sandbox");
        assert!(!format!("{command:?}").contains("password"));
    }

    #[test]
    fn container_conflict_channel_accepts_only_verified_source_evidence() {
        // Causes: C1 exact local HEAD differs from desired, C2 exact 40-hex
        // third remote ref, C3 absent ref with update prior, C4 desired/prior/
        // malformed or untrusted channel text. Effects: E1-E3 use the one
        // permanent rejection contract; E4 remains unavailable and can never
        // close the terminal Session as rejected.
        // | Rule | observation | expectation | effect |
        // | P1 | LOCAL:other | any | E1 local mismatch |
        // | P2 | REF:third | create/update | E2 remote change |
        // | P3 | ABSENT | update | E3 remote absence |
        // | P4 | desired/prior/malformed/empty | any | E4 unavailable |
        let desired = "1".repeat(40);
        let prior = "2".repeat(40);
        let third = "3".repeat(40);
        let create = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: desired.clone(),
            expected_prior_commit: None,
        };
        let update = pc::RepositoryPublicationExpectation {
            expected_prior_commit: Some(prior.clone()),
            ..create.clone()
        };
        assert_eq!(
            decode_publication_rejection(&format!("LOCAL:{prior}\n"), &create).unwrap(),
            pc::RepositoryPublicationRejection::LocalCommitMismatch {
                observed_commit: prior.clone()
            },
            "P1"
        );
        assert_eq!(
            decode_publication_rejection(&format!("REF:{third}\n"), &create).unwrap(),
            pc::RepositoryPublicationRejection::RemoteRefChanged {
                observed_commit: third.clone()
            },
            "P1"
        );
        assert_eq!(
            decode_publication_rejection("ABSENT\n", &update).unwrap(),
            pc::RepositoryPublicationRejection::RemoteRefAbsent,
            "P2"
        );
        for (wire, expectation) in [
            (format!("REF:{desired}\n"), &create),
            (format!("REF:{prior}\n"), &update),
            ("ABSENT\n".into(), &create),
            ("REF:short\n".into(), &update),
            ("REF:3\nBearer token".into(), &update),
            (format!("LOCAL:{desired}\n"), &create),
            ("LOCAL:short\n".into(), &create),
            ("LOCAL:3\nBearer token".into(), &create),
            (String::new(), &update),
        ] {
            assert!(
                decode_publication_rejection(&wire, expectation).is_err(),
                "P3"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn push_observation_failure_cannot_masquerade_as_an_absent_remote_ref() {
        use std::os::unix::fs::PermissionsExt as _;

        // Cause/effect table: R1 transient read then prior -> one leased push;
        // R2 every read fails -> unavailable (43); R3 third commit -> bounded
        // exact ref evidence (42); R4 absent create lease -> one create; R5
        // already desired -> replay; R6 failed push then desired -> response-
        // loss success; R7 failed push then third -> exact rejection evidence;
        // R8 failed push with unchanged prior -> unavailable; R9 absent update
        // prior -> typed absent evidence; R10 malformed OID and R11 multiple
        // ref lines -> unavailable; R12 exact different local HEAD -> typed
        // rejection before any remote read, R13 malformed local HEAD ->
        // unavailable before any remote read.
        // Git diagnostic output is suppressed; only the bounded observation
        // marker may cross the Sandbox channel.
        let root = tempfile::tempdir().unwrap();
        let git = root.path().join("git");
        std::fs::write(
            &git,
            r#"#!/bin/sh
case " $* " in
  *" check-ref-format "*) exit 0 ;;
  *" symbolic-ref "*) printf 'awf/work\n'; exit 0 ;;
  *" rev-parse "*) printf '%s\n' "$FAKE_LOCAL_COMMIT"; exit 0 ;;
  *" ls-remote "*)
    count=$(cat "$FAKE_COUNT_FILE" 2>/dev/null || printf 0)
    count=$((count + 1))
    printf '%s\n' "$count" > "$FAKE_COUNT_FILE"
    if test "$count" -le "$FAKE_FAILS"; then exit 128; fi
    sha=$FAKE_REMOTE_SHA
    if test -e "$FAKE_PUSH_FILE" && test -n "$FAKE_AFTER_SHA"; then sha=$FAKE_AFTER_SHA; fi
    if test -n "$sha"; then printf '%s\trefs/heads/awf/work\n' "$sha"; fi
    exit 0 ;;
  *" push "*) : > "$FAKE_PUSH_FILE"; exit "${FAKE_PUSH_CODE:-0}" ;;
esac
exit 99
"#,
        )
        .unwrap();
        std::fs::set_permissions(&git, std::fs::Permissions::from_mode(0o755)).unwrap();
        let desired = "1".repeat(40);
        let prior = "2".repeat(40);
        let third = "3".repeat(40);
        let malformed = format!("{}X", "3".repeat(39));
        let multiple = format!("{third}\n{prior}");
        for (rule, failures, remote, after, expected_prior, push_code, code, pushes, reads) in [
            (
                "R1",
                1,
                prior.as_str(),
                "",
                Some(prior.as_str()),
                0,
                0,
                true,
                2,
            ),
            (
                "R2",
                3,
                prior.as_str(),
                "",
                Some(prior.as_str()),
                0,
                43,
                false,
                3,
            ),
            (
                "R3",
                0,
                third.as_str(),
                "",
                Some(prior.as_str()),
                0,
                42,
                false,
                1,
            ),
            ("R4", 0, "", "", None, 0, 0, true, 1),
            (
                "R5",
                0,
                desired.as_str(),
                "",
                Some(prior.as_str()),
                0,
                0,
                false,
                1,
            ),
            (
                "R6",
                0,
                prior.as_str(),
                desired.as_str(),
                Some(prior.as_str()),
                128,
                0,
                true,
                2,
            ),
            (
                "R7",
                0,
                prior.as_str(),
                third.as_str(),
                Some(prior.as_str()),
                128,
                42,
                true,
                2,
            ),
            (
                "R8",
                0,
                prior.as_str(),
                prior.as_str(),
                Some(prior.as_str()),
                128,
                43,
                true,
                2,
            ),
            ("R9", 0, "", "", Some(prior.as_str()), 0, 42, false, 1),
            (
                "R10",
                0,
                malformed.as_str(),
                "",
                Some(prior.as_str()),
                0,
                43,
                false,
                1,
            ),
            (
                "R11",
                0,
                multiple.as_str(),
                "",
                Some(prior.as_str()),
                0,
                43,
                false,
                1,
            ),
            ("R12", 0, "", "", None, 0, 42, false, 0),
            ("R13", 0, "", "", None, 0, 43, false, 0),
        ] {
            let count_file = root.path().join(format!("{rule}-count"));
            let push_file = root.path().join(format!("{rule}-push"));
            let expectation = pc::RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: desired.clone(),
                expected_prior_commit: expected_prior.map(str::to_owned),
            };
            let command = repository_push_command(&plan(), &expectation, "/unused/helper");
            let output = std::process::Command::new("sh")
                .args(command.argv.iter().skip(1))
                .env(
                    "PATH",
                    format!(
                        "{}:{}",
                        root.path().display(),
                        std::env::var("PATH").unwrap_or_default()
                    ),
                )
                .env(
                    "FAKE_LOCAL_COMMIT",
                    match rule {
                        "R12" => prior.as_str(),
                        "R13" => malformed.as_str(),
                        _ => desired.as_str(),
                    },
                )
                .env("FAKE_REMOTE_SHA", remote)
                .env("FAKE_AFTER_SHA", after)
                .env("FAKE_PUSH_CODE", push_code.to_string())
                .env("FAKE_FAILS", failures.to_string())
                .env("FAKE_COUNT_FILE", &count_file)
                .env("FAKE_PUSH_FILE", &push_file)
                .output()
                .unwrap();
            assert_eq!(output.status.code(), Some(code), "{rule}: {output:?}");
            let expected_output = match rule {
                "R3" | "R7" => format!("REF:{third}\n"),
                "R9" => "ABSENT\n".to_owned(),
                "R12" => format!("LOCAL:{prior}\n"),
                _ => String::new(),
            };
            assert_eq!(
                output.stdout,
                expected_output.as_bytes(),
                "{rule}: safe wire"
            );
            assert_eq!(push_file.exists(), pushes, "{rule}");
            if reads == 0 {
                assert!(!count_file.exists(), "{rule}: no remote request");
            } else {
                assert_eq!(
                    std::fs::read_to_string(count_file).unwrap().trim(),
                    reads.to_string(),
                    "{rule}"
                );
            }
        }
    }

    #[tokio::test]
    async fn credential_service_is_exact_target_bound_and_redacted() {
        let credential =
            pc::RepositoryHttpBasicCredential::new("user".to_owned(), "secret".to_owned());
        let service = OperationRepositoryCredential::new(
            "https://github.example:8443/owner/repo.git",
            &credential,
            awaken_time::system_clock(),
        )
        .unwrap();
        let response = service
            .handle(SandboxControlRequest::RepositoryGitCredentialGet {
                query: RepositoryGitCredentialQuery {
                    protocol: "https".into(),
                    host: "github.example:8443".into(),
                    path: "owner/repo.git".into(),
                },
            })
            .await;
        assert!(matches!(
            response,
            SandboxControlResponse::RepositoryGitCredential { .. }
        ));
        let denied = service
            .handle(SandboxControlRequest::RepositoryGitCredentialGet {
                query: RepositoryGitCredentialQuery {
                    protocol: "https".into(),
                    host: "github.example:8443".into(),
                    path: "owner/other.git".into(),
                },
            })
            .await;
        assert!(matches!(denied, SandboxControlResponse::Denied));
        assert!(!format!("{response:?}").contains("secret"));
    }
}
