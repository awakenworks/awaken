// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Static owner: the one private Git process constructor and command-scoped HTTP Basic
// @formal reason: credential holder beneath the local Repository transport. Dynamic path: network commands
// @formal reason: admit the exact protocol and authority and remove ambient Git, SSH, proxy, TLS, trace,
// @formal reason: and credential configuration; local commands remove explicit Repository-selection
// @formal reason: variables plus Awaken and askpass credential variables, use the caller-provided working
// @formal reason: directory when supplied, fence discovery at its parent, and otherwise execute the caller-selected argument-only local
// @formal reason: operation. Credentials are released only through the exact protocol-plus-authority
// @formal reason: helper. Windows deep clones use a short owned network repository followed by an
// @formal reason: independent credential-free local clone; this module selects no branch, commit,
// @formal reason: push, ref, CAS outcome, or receipt. Evidence:
// @formal reason: credential.plaintext_boundary.exact_holder and the parent Git transport decision-table
// @formal reason: and redirect tests; this is a product-requirement classification, not a direct
// @formal reason: formal-proof claim.
//! Command-scoped Git process and credential boundary for local Repository transport.
//!
//! This private module admits one exact network endpoint and invokes Git.
//! Network commands remove ambient Git, SSH, proxy, TLS, trace, and credential
//! configuration; local commands remove explicit Repository-selection variables
//! plus Awaken and askpass credential variables, use the caller-provided working
//! directory when supplied, and otherwise execute the caller-selected
//! argument-only local operation. Windows clone invocation compatibility also
//! lives here, beneath the caller's exact realization plan. It owns no push policy, remote-write
//! selection, CAS classification, receipt, queue, or durable state.

use std::path::Path;

use crate::SandboxError;

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdmittedGitEndpoint {
    protocol: String,
    authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AdmittedGitTransport {
    protocol: String,
    credential_endpoint: Option<AdmittedGitEndpoint>,
}

pub(super) fn validate_credentialed_git_transport(
    url: &str,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<AdmittedGitTransport, SandboxError> {
    if credential.is_none() && cfg!(any(test, feature = "test-support")) && test_local_path(url) {
        return Ok(AdmittedGitTransport {
            protocol: "file".into(),
            credential_endpoint: None,
        });
    }
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_) => {
            return Err(SandboxError::invalid(
                "repository transport must be an absolute HTTPS endpoint",
            ));
        }
    };
    if credential.is_none() {
        if parsed.scheme() == "file"
            && cfg!(any(test, feature = "test-support"))
            && parsed.host().is_none()
            && parsed.username().is_empty()
            && parsed.password().is_none()
        {
            return Ok(AdmittedGitTransport {
                protocol: "file".into(),
                credential_endpoint: None,
            });
        }
        if parsed.scheme() != "https"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.host().is_none()
            || url.bytes().any(|byte| byte.is_ascii_whitespace())
        {
            return Err(SandboxError::invalid(
                "anonymous repository transport must be HTTPS without embedded user info",
            ));
        }
        return Ok(AdmittedGitTransport {
            protocol: "https".into(),
            credential_endpoint: None,
        });
    }
    let credential = credential.expect("credential checked above");
    if parsed.scheme() != "https"
        && !(credential.is_gateway_capability() && parsed.scheme() == "http")
    {
        return Err(SandboxError::invalid(
            "credentialed repository URL must use its admitted HTTP transport without embedded user info",
        ));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host().is_none()
        || url.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(SandboxError::invalid(
            "credentialed repository URL must use its admitted HTTP transport without embedded user info",
        ));
    }
    if [credential.expose_username(), credential.expose_password()]
        .iter()
        .any(|value| {
            value.is_empty()
                || value
                    .bytes()
                    .any(|byte| matches!(byte, b'\0' | b'\r' | b'\n'))
        })
    {
        return Err(SandboxError::invalid(
            "repository HTTP Basic credentials contain an invalid protocol value",
        ));
    }
    let host = match parsed.host().expect("host checked above") {
        url::Host::Domain(host) => host.to_owned(),
        url::Host::Ipv4(host) => host.to_string(),
        url::Host::Ipv6(host) => format!("[{host}]"),
    };
    let authority = parsed
        .port()
        .map_or(host.clone(), |port| format!("{host}:{port}"));
    let protocol = parsed.scheme().to_owned();
    Ok(AdmittedGitTransport {
        protocol: protocol.clone(),
        credential_endpoint: Some(AdmittedGitEndpoint {
            protocol,
            authority,
        }),
    })
}

fn test_local_path(url: &str) -> bool {
    let path = Path::new(url);
    if !path.is_absolute() {
        return false;
    }
    #[cfg(windows)]
    {
        matches!(
            path.components().next(),
            Some(std::path::Component::Prefix(prefix))
                if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::VerbatimDisk(_))
        )
    }
    #[cfg(not(windows))]
    {
        true
    }
}

pub(crate) const GIT_CREDENTIAL_USERNAME_ENV: &str = "AWAKEN_GIT_CREDENTIAL_USERNAME";
pub(crate) const GIT_CREDENTIAL_PASSWORD_ENV: &str = "AWAKEN_GIT_CREDENTIAL_PASSWORD";
const GIT_CREDENTIAL_PROTOCOL_ENV: &str = "AWAKEN_GIT_CREDENTIAL_PROTOCOL";
const GIT_CREDENTIAL_AUTHORITY_ENV: &str = "AWAKEN_GIT_CREDENTIAL_AUTHORITY";
const GIT_CREDENTIAL_HELPER: &str = "!f() { [ \"$1\" = get ] || exit 0; protocol=; host=; while IFS='=' read -r key value; do [ -n \"$key\" ] || break; case \"$key\" in protocol) protocol=$value ;; host) host=$value ;; esac; done; [ \"$protocol\" = \"$AWAKEN_GIT_CREDENTIAL_PROTOCOL\" ] || exit 0; [ \"$host\" = \"$AWAKEN_GIT_CREDENTIAL_AUTHORITY\" ] || exit 0; printf '%s\\n' \"username=$AWAKEN_GIT_CREDENTIAL_USERNAME\" \"password=$AWAKEN_GIT_CREDENTIAL_PASSWORD\"; }; f";

enum GitAuthentication<'a> {
    Local,
    Network {
        credential: Option<&'a awaken_provisioning_contract::RepositoryHttpBasicCredential>,
        transport: AdmittedGitTransport,
        alternate_objects: Option<&'a Path>,
        discovery_ceiling: Option<&'a Path>,
    },
}

/// Configuration every spawned Git receives, before any branch-specific option
/// and before the caller's arguments.
///
/// Invariant: an effect spawns no process that outlives it. Git otherwise
/// detaches `gc --auto` / `maintenance` after clone, fetch, commit, and
/// receive-pack, and that child keeps writing under this process's `TMPDIR`
/// (the effect's `repository-tmp`) after the effect has returned. Every Git
/// invocation therefore disables automatic maintenance in full: no auto gc,
/// no detached gc even if one were triggered, no scheduled maintenance, and
/// no post-fetch commit-graph write.
const GIT_HOST_PROCESS_FLOOR: &[&str] = &[
    "-c",
    "gc.auto=0",
    "-c",
    "gc.autoDetach=false",
    "-c",
    "maintenance.auto=false",
    "-c",
    "fetch.writeCommitGraph=false",
    "-c",
    "core.hooksPath=/dev/null",
    "-c",
    "core.fsmonitor=false",
    "-c",
    "submodule.recurse=false",
];

/// The fully composed Git process for one invocation. The returned directory,
/// when present, is the Network branch's isolated working directory and must
/// live until the process has exited.
#[expect(
    clippy::disallowed_methods,
    reason = "the local Sandbox provider passes the host PATH and passthrough variables to the processes it launches"
)]
fn compose_git_command(
    cwd: Option<&Path>,
    args: &[&str],
    authentication: GitAuthentication<'_>,
) -> Result<(std::process::Command, Option<tempfile::TempDir>), SandboxError> {
    let git = awaken_local_process::resolve_operator_executable(
        "git",
        &std::env::var_os("PATH").unwrap_or_default(),
    )
    .ok_or_else(|| {
        SandboxError::unsupported("repository Git is unavailable on the Worker's absolute PATH")
    })?;
    let mut command = std::process::Command::new(git);
    command.args(GIT_HOST_PROCESS_FLOOR);
    // Git for Windows keeps its legacy path limit unless this process-scoped
    // option is set. A bounded Sandbox root can still put clone's internal
    // `.git/objects/pack/*.keep` beyond that limit; the operator's global Git
    // configuration is intentionally excluded from network commands below.
    #[cfg(windows)]
    command.args(["-c", "core.longpaths=true"]);
    command
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove(GIT_CREDENTIAL_USERNAME_ENV)
        .env_remove(GIT_CREDENTIAL_PASSWORD_ENV)
        .env_remove(GIT_CREDENTIAL_PROTOCOL_ENV)
        .env_remove(GIT_CREDENTIAL_AUTHORITY_ENV)
        .env_remove("GIT_ASKPASS")
        .env_remove("SSH_ASKPASS")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_OBJECT_DIRECTORY")
        .env_remove("GIT_ALTERNATE_OBJECT_DIRECTORIES")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_NAMESPACE")
        .env_remove("GIT_CEILING_DIRECTORIES");
    let mut isolated_cwd = None;
    if let GitAuthentication::Network {
        credential,
        transport,
        alternate_objects,
        discovery_ceiling,
    } = authentication
    {
        let isolated_cwd = isolated_cwd.insert(
            tempfile::tempdir()
                .map_err(|error| SandboxError::backend("compose_git_command", error))?,
        );
        command
            .current_dir(isolated_cwd.path())
            .env("HOME", isolated_cwd.path())
            .env("XDG_CONFIG_HOME", isolated_cwd.path().join("xdg"))
            .env("CURL_HOME", isolated_cwd.path().join("curl"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_SYSTEM",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env(
                "GIT_CONFIG_GLOBAL",
                if cfg!(windows) { "NUL" } else { "/dev/null" },
            )
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env_remove("GIT_CONFIG")
            .env_remove("GIT_CONFIG_PARAMETERS")
            .env_remove("GIT_CONFIG_COUNT")
            .env_remove("GIT_ALLOW_PROTOCOL")
            .env_remove("GIT_PROTOCOL_FROM_USER")
            .env_remove("GIT_EXEC_PATH")
            .env_remove("GIT_TEMPLATE_DIR")
            .env_remove("GIT_SSH")
            .env_remove("GIT_SSH_COMMAND")
            .env_remove("GIT_SSH_VARIANT")
            .env_remove("GIT_PROXY_COMMAND")
            .env_remove("SSH_AUTH_SOCK")
            .env_remove("SSH_AGENT_PID")
            .env_remove("SSH_ASKPASS_REQUIRE")
            .env_remove("NETRC")
            .env_remove("GIT_CURL_VERBOSE")
            .env_remove("CURL_CA_BUNDLE")
            .env_remove("SSL_CERT_FILE")
            .env_remove("SSL_CERT_DIR")
            .env_remove("http_proxy")
            .env_remove("https_proxy")
            .env_remove("all_proxy")
            .env_remove("no_proxy")
            .env_remove("HTTP_PROXY")
            .env_remove("HTTPS_PROXY")
            .env_remove("ALL_PROXY")
            .env_remove("NO_PROXY")
            .args([
                "-c",
                "credential.helper=",
                "-c",
                "credential.interactive=never",
                "-c",
                "credential.useHttpPath=false",
                "-c",
                "http.extraHeader=",
                "-c",
                "protocol.allow=never",
            ]);
        let allowed_protocol = format!("protocol.{}.allow=always", transport.protocol);
        command.args(["-c", allowed_protocol.as_str()]);
        for (name, _) in std::env::vars_os() {
            let name_text = name.to_string_lossy();
            if name_text.starts_with("GIT_CONFIG_KEY_")
                || name_text.starts_with("GIT_CONFIG_VALUE_")
                || name_text.starts_with("GIT_TRACE")
                || name_text.starts_with("GIT_SSL_")
                || name_text.starts_with("GIT_PROXY_SSL_")
                || name_text.starts_with("GIT_REDIRECT_")
            {
                command.env_remove(name);
            }
        }
        if let Some(objects) = alternate_objects {
            let alternate = std::env::join_paths([objects]).map_err(|_| {
                SandboxError::invalid("repository object path cannot form a Git alternate")
            })?;
            command.env("GIT_ALTERNATE_OBJECT_DIRECTORIES", alternate);
        }
        if let (Some(credential), Some(endpoint)) = (credential, transport.credential_endpoint) {
            let helper_config = format!("credential.helper={GIT_CREDENTIAL_HELPER}");
            command
                .args(["-c", helper_config.as_str()])
                .env(GIT_CREDENTIAL_USERNAME_ENV, credential.expose_username())
                .env(GIT_CREDENTIAL_PASSWORD_ENV, credential.expose_password())
                .env(GIT_CREDENTIAL_PROTOCOL_ENV, endpoint.protocol)
                .env(GIT_CREDENTIAL_AUTHORITY_ENV, endpoint.authority);
        }
        if let Some(ceiling) = discovery_ceiling {
            command.env(
                "GIT_CEILING_DIRECTORIES",
                std::path::absolute(ceiling)
                    .map_err(|error| SandboxError::backend("compose_git_command", error))?,
            );
        }
    } else if let Some(dir) = cwd {
        command.current_dir(dir);
        // A missing/unrecognizable repository must never select an ancestor's
        // checkout. The caller supplies this operation's repository root.
        let absolute = std::path::absolute(dir)
            .map_err(|error| SandboxError::backend("compose_git_command", error))?;
        if let Some(parent) = absolute.parent() {
            command.env("GIT_CEILING_DIRECTORIES", parent);
        }
    }
    command.args(args);
    Ok((command, isolated_cwd))
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;

    fn agent_git(repository: &Path, args: &[&str]) -> std::process::Output {
        // Deliberately bypass git_command_output's process-local longpaths
        // floor. The published repository must also work for the Agent's Git.
        let mut command = std::process::Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        let output = command
            .current_dir(repository)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "NUL")
            .env("GIT_CEILING_DIRECTORIES", repository.parent().unwrap())
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "Agent Git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }

    #[test]
    fn staged_clone_over_git_directory_limit_preserves_exact_plan_and_replay() {
        // R1 ASCII and R2 multibyte paths fit the Windows process-directory
        // limit but exceed Git's UTF-8 GIT_DIR budget. Each row exercises the
        // actual staged publication, with default branch, named branch, and
        // detached commit plans. Retain must preserve subsequent user edits.
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        run_git(Some(&source), &["init", "-q", "--initial-branch=main"]).unwrap();
        run_git(Some(&source), &["config", "user.name", "fixture"]).unwrap();
        run_git(
            Some(&source),
            &["config", "user.email", "fixture@example.invalid"],
        )
        .unwrap();
        std::fs::write(source.join("a.txt"), b"first").unwrap();
        run_git(Some(&source), &["add", "a.txt"]).unwrap();
        run_git(Some(&source), &["commit", "-qm", "first"]).unwrap();
        let first = git_stdout(Some(&source), &["rev-parse", "HEAD"]).unwrap();
        run_git(Some(&source), &["checkout", "-qb", "fixture-branch"]).unwrap();
        std::fs::write(source.join("a.txt"), b"second").unwrap();
        run_git(Some(&source), &["commit", "-qam", "second"]).unwrap();
        let second = git_stdout(Some(&source), &["rev-parse", "HEAD"]).unwrap();
        let url = url::Url::from_directory_path(&source).unwrap().to_string();
        let unicode_segment = "仓".repeat(16);
        for (style, workspace_units, unicode) in [
            ("direct", 170, ""),
            ("ascii", 210, ""),
            ("ascii-limit", 214, ""),
            ("unicode", 190, unicode_segment.as_str()),
        ] {
            for (index, branch, commit, expected, bytes) in [
                (0, None, None, second.trim(), b"second".as_slice()),
                (1, Some("main"), None, first.trim(), b"first".as_slice()),
                (
                    3,
                    Some("main"),
                    Some(second.trim()),
                    second.trim(),
                    b"second".as_slice(),
                ),
                (
                    2,
                    None,
                    Some(first.trim()),
                    first.trim(),
                    b"first".as_slice(),
                ),
            ] {
                let prefix = temporary.path().join(format!("{style}-{index}"));
                let fixed = prefix.to_string_lossy().encode_utf16().count()
                    + 3
                    + "jail".len()
                    + "workspace".len();
                let segment = format!(
                    "{unicode}{}",
                    "x".repeat(workspace_units - fixed - unicode.encode_utf16().count())
                );
                let jail = prefix.join(segment).join("jail");
                let workspace = jail.join("workspace");
                std::fs::create_dir_all(&workspace).unwrap();
                assert_eq!(
                    workspace.to_string_lossy().encode_utf16().count(),
                    workspace_units
                );
                let root = crate::IsolatedRoot::new(&jail);
                let provision = |intent| {
                    crate::git_transport::provision_repo_at(
                        &root,
                        "workspace/repo",
                        &url,
                        branch,
                        commit,
                        None,
                        intent,
                    )
                };
                provision(awaken_provisioning_contract::RepositoryRealizationIntent::Initialize)
                    .unwrap_or_else(|error| panic!("{style}/{index}: {error}"));
                let destination = workspace.join("repo");
                assert_eq!(
                    git_stdout(Some(&destination), &["rev-parse", "HEAD"])
                        .unwrap()
                        .trim(),
                    expected
                );
                assert_eq!(std::fs::read(destination.join("a.txt")).unwrap(), bytes);
                assert_eq!(
                    git_stdout(Some(&destination), &["remote", "get-url", "origin"])
                        .unwrap()
                        .trim(),
                    url
                );
                assert!(
                    !destination.join(".git/objects/info/alternates").exists(),
                    "published objects do not depend on the removed private clone"
                );
                assert_eq!(
                    git_stdout(
                        Some(&destination),
                        &["symbolic-ref", "refs/remotes/origin/HEAD"]
                    )
                    .unwrap()
                    .trim(),
                    "refs/remotes/origin/fixture-branch",
                    "remote default branch is independent of the selected checkout"
                );
                let symbolic = git_stdout(Some(&destination), &["symbolic-ref", "--short", "HEAD"]);
                if commit.is_some() && branch.is_none() {
                    assert!(symbolic.is_err(), "exact commit plan stays detached");
                } else {
                    assert_eq!(symbolic.unwrap().trim(), branch.unwrap_or("fixture-branch"));
                }
                // Each path/checkout row must support ordinary Agent writes,
                // including new loose objects beyond MAX_PATH. A successful
                // clone through our process-only flag is insufficient.
                let agent_output = format!("{}.txt", "agent-output-".repeat(8));
                assert!(
                    destination
                        .join(&agent_output)
                        .to_string_lossy()
                        .encode_utf16()
                        .count()
                        > 260
                );
                std::fs::write(destination.join(&agent_output), b"agent edit\n").unwrap();
                agent_git(&destination, &["add", "-N", &agent_output]);
                let diff = agent_git(
                    &destination,
                    &["diff", "--no-ext-diff", "--", &agent_output],
                );
                assert!(String::from_utf8_lossy(&diff.stdout).contains("+agent edit\n"));
                assert_eq!(
                    agent_git(
                        &destination,
                        &["config", "--local", "--get", "core.longpaths"]
                    )
                    .stdout,
                    b"true\n"
                );
                std::fs::write(destination.join("a.txt"), b"user edit").unwrap();
                provision(awaken_provisioning_contract::RepositoryRealizationIntent::Retain)
                    .unwrap();
                assert_eq!(
                    std::fs::read(destination.join("a.txt")).unwrap(),
                    b"user edit"
                );
                assert_eq!(
                    std::fs::read(destination.join(&agent_output)).unwrap(),
                    b"agent edit\n"
                );
                assert_eq!(
                    std::fs::read_dir(&workspace).unwrap().count(),
                    1,
                    "owned stage removed"
                );
            }
        }
    }

    #[test]
    fn unrepresentable_clone_directory_is_permanent_and_transport_admission_stays_first() {
        let temporary = tempfile::tempdir().unwrap();
        let root_units = temporary.path().to_string_lossy().encode_utf16().count();
        let parent = temporary.path().join("x".repeat(234 - root_units - 1));
        std::fs::create_dir_all(&parent).unwrap();
        let stage = tempfile::Builder::new()
            .prefix("stage-")
            .rand_bytes(6)
            .tempdir_in(&parent)
            .unwrap();
        assert_eq!(stage.path().to_string_lossy().encode_utf16().count(), 247);
        let error = credentialed_git_clone(
            "https://network-must-not-run.invalid/repo",
            &stage,
            None,
            None,
        )
        .expect_err("unsupported working directory rejects before network Git");
        assert!(matches!(error, SandboxError::Unsupported(_)), "{error}");
        let error = credentialed_git_clone(
            "ssh://network-must-not-run.invalid/repo",
            &stage,
            None,
            None,
        )
        .expect_err("private file transfer must not bypass external transport admission");
        assert!(matches!(error, SandboxError::Invalid(_)), "{error}");
        assert_eq!(std::fs::read_dir(stage.path()).unwrap().count(), 0);
    }

    #[test]
    fn credential_isolated_clone_accepts_git_internal_paths_beyond_max_path() {
        // Cause/effect table: C1=the test-only file transport is admitted;
        // C2=the clone destination is below MAX_PATH but Git's pack keep file
        // crosses it; C3=network Git excludes operator global configuration.
        // E1=the exact source HEAD is cloned; E2=the deep stage remains under
        // the caller-selected root. R1=C1+C2+C3 -> E1+E2. A real Windows Git
        // clone with core.longpaths=false fails at its keep file for this path.
        const STAGE_PATH_UNITS: usize = 207;
        const LEGACY_PATH_LIMIT: usize = 260;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        std::fs::create_dir(&source).unwrap();
        run_git(Some(&source), &["init", "-q"]).unwrap();
        std::fs::write(source.join("a.txt"), b"fixture").unwrap();
        run_git(Some(&source), &["add", "a.txt"]).unwrap();
        run_git(
            Some(&source),
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .unwrap();
        let url = url::Url::from_directory_path(&source).unwrap().to_string();
        let stage_name = ".awaken-repository-test.stage";
        let fixed_length = root.path().to_string_lossy().encode_utf16().count()
            + 3
            + "workspace".len()
            + stage_name.len();
        let segment_length = STAGE_PATH_UNITS
            .checked_sub(fixed_length)
            .expect("Windows temporary root leaves room for the test stage");
        let parent = root
            .path()
            .join("x".repeat(segment_length))
            .join("workspace");
        std::fs::create_dir_all(&parent).unwrap();
        let stage = parent.join(stage_name);
        assert_eq!(
            stage.to_string_lossy().encode_utf16().count(),
            STAGE_PATH_UNITS,
            "R1/C2"
        );
        assert!(
            stage
                .join(".git/objects/pack/pack-")
                .to_string_lossy()
                .encode_utf16()
                .count()
                + 45
                > LEGACY_PATH_LIMIT,
            "R1/C2"
        );
        credentialed_git_run(
            &url,
            &[
                "clone",
                "--no-checkout",
                url.as_str(),
                stage.to_str().unwrap(),
            ],
            None,
        )
        .unwrap();
        assert_eq!(
            git_stdout(Some(&stage), &["rev-parse", "HEAD"]).unwrap(),
            git_stdout(Some(&source), &["rev-parse", "HEAD"]).unwrap(),
            "R1/E1+E2"
        );
    }
}

fn git_command_output(
    cwd: Option<&Path>,
    args: &[&str],
    authentication: GitAuthentication<'_>,
) -> Result<std::process::Output, SandboxError> {
    let redaction = match &authentication {
        GitAuthentication::Local => None,
        GitAuthentication::Network { credential, .. } => *credential,
    };
    let (mut command, isolated_cwd) = compose_git_command(cwd, args, authentication)?;
    let output = command.output().map_err(|error| {
        SandboxError::backend(
            "git_command_output",
            format!("git {}: {error}", args.first().unwrap_or(&"")),
        )
    })?;
    if !output.status.success() {
        let mut detail = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if let Some(credential) = redaction {
            for secret in [credential.expose_password(), credential.expose_username()] {
                if !secret.is_empty() {
                    detail = detail.replace(secret, "[REDACTED]");
                }
            }
        }
        return Err(SandboxError::backend(
            "git_command_output",
            format!("git {} failed: {}", args.first().unwrap_or(&""), detail),
        ));
    }
    drop(isolated_cwd);
    Ok(output)
}

fn git_run(cwd: Option<&Path>, args: &[&str]) -> Result<std::process::Output, SandboxError> {
    git_command_output(cwd, args, GitAuthentication::Local)
}

pub(crate) fn run_git(cwd: Option<&Path>, args: &[impl AsRef<str>]) -> Result<(), SandboxError> {
    let borrowed: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    git_run(cwd, &borrowed).map(|_| ())
}

pub(super) fn credentialed_git_run(
    url: &str,
    args: &[impl AsRef<str>],
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<(), SandboxError> {
    credentialed_git_run_inner(url, args, credential, None)
}

pub(super) fn credentialed_git_clone(
    url: &str,
    stage: &tempfile::TempDir,
    initial_branch: Option<&str>,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<(), SandboxError> {
    // Admit the original endpoint before considering an internal file transfer.
    validate_credentialed_git_transport(url, credential)?;
    let stage_path = std::path::absolute(stage.path())
        .map_err(|error| SandboxError::backend("credentialed_git_clone", error))?;
    #[cfg(windows)]
    {
        if stage_path
            .join(".git/objects")
            .to_string_lossy()
            .encode_utf16()
            .count()
            >= 260
        {
            return Err(SandboxError::unsupported(
                "Windows Git repository discovery requires staging metadata paths shorter than 260 UTF-16 units; use a shorter sandbox root or Repository mount path",
            ));
        }
        // Initial repository discovery probes .git/objects before reading
        // core.longpaths; reject an unrecognizable stage before network I/O.
        // Git for Windows also explicitly rejects GIT_DIR UTF-8 byte lengths above
        // PATH_MAX - 40, independently of core.longpaths. Unicode paths can
        // cross that budget before their Windows UTF-16 process-directory limit.
        if stage_path.join(".git").to_string_lossy().len() > 220 {
            return windows_private_clone(url, stage, initial_branch, credential);
        }
    }
    let mut args = vec!["clone", "--no-checkout"];
    // Persist the Windows setting in the new repository as well as using the
    // command floor above. Agent Git runs after publication do not inherit our
    // process flags and may create long loose-object or working-tree paths.
    #[cfg(windows)]
    args.extend(["--config", "core.longpaths=true"]);
    if let Some(branch) = initial_branch {
        args.extend(["--branch", branch]);
    }
    let path = stage_path.to_string_lossy();
    args.extend(["--", url, &path]);
    credentialed_git_run(url, &args, credential)
}

#[cfg(windows)]
fn windows_private_clone(
    url: &str,
    stage: &tempfile::TempDir,
    initial_branch: Option<&str>,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<(), SandboxError> {
    let stage_path = std::path::absolute(stage.path())
        .map_err(|error| SandboxError::backend("credentialed_git_clone", error))?;
    let private = tempfile::tempdir()
        .map_err(|error| SandboxError::backend("credentialed_git_clone", error))?;
    let repository = private.path().join("repository.git");
    let source = repository.to_string_lossy();
    let target = stage_path.to_string_lossy();
    let result = (|| {
        if source.len() > 220 {
            return Err(SandboxError::unsupported(
                "Windows Git network clone requires a temporary Git directory within 220 UTF-8 bytes; use a shorter TEMP directory",
            ));
        }
        let mut network = vec!["clone", "--bare"];
        let mut local = vec!["clone", "--local", "--no-hardlinks", "--no-checkout"];
        local.extend(["--config", "core.longpaths=true"]);
        if let Some(branch) = initial_branch {
            // Preserve the original remote HEAD in the bare source; only the
            // final local clone selects the caller's checkout branch.
            local.extend(["--branch", branch]);
        }
        network.extend(["--", url, &source]);
        credentialed_git_run(url, &network, credential)?;
        local.extend(["--", &source, &target]);
        // This file source is an owned, freshly cloned private repository, not
        // a caller-admitted file URL. Reuse the sanitized network environment
        // without a credential; ambient URL rewrites/helpers remain excluded.
        let isolated_local = |args: &[&str]| {
            git_command_output(
                None,
                args,
                GitAuthentication::Network {
                    credential: None,
                    transport: AdmittedGitTransport {
                        protocol: "file".into(),
                        credential_endpoint: None,
                    },
                    alternate_objects: None,
                    discovery_ceiling: stage_path.parent(),
                },
            )
            .map(|_| ())
        };
        isolated_local(&local)?;
        isolated_local(&["-C", &target, "remote", "set-url", "origin", url])
    })();
    // Neither Git metadata nor object files in the stage depend on this source:
    // --no-hardlinks copies objects, and the origin was restored before return.
    match private.close() {
        Ok(()) => result,
        Err(cleanup) => Err(match result {
            Ok(()) => SandboxError::backend("credentialed_git_clone", cleanup),
            Err(error) => error.map_detail(|detail| {
                format!("{detail}; remove owned private clone source: {cleanup}")
            }),
        }),
    }
}

pub(super) fn credentialed_git_run_with_alternate(
    url: &str,
    args: &[impl AsRef<str>],
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
    alternate_objects: &Path,
) -> Result<(), SandboxError> {
    credentialed_git_run_inner(url, args, credential, Some(alternate_objects))
}

fn credentialed_git_run_inner(
    url: &str,
    args: &[impl AsRef<str>],
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
    alternate_objects: Option<&Path>,
) -> Result<(), SandboxError> {
    let transport = validate_credentialed_git_transport(url, credential)?;
    let borrowed: Vec<&str> = args.iter().map(AsRef::as_ref).collect();
    git_command_output(
        None,
        &borrowed,
        GitAuthentication::Network {
            credential,
            transport,
            alternate_objects,
            discovery_ceiling: None,
        },
    )
    .map(|_| ())
}

pub(super) fn credentialed_git_stdout(
    url: &str,
    args: &[&str],
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<String, SandboxError> {
    let transport = validate_credentialed_git_transport(url, credential)?;
    Ok(String::from_utf8_lossy(
        &git_command_output(
            None,
            args,
            GitAuthentication::Network {
                credential,
                transport,
                alternate_objects: None,
                discovery_ceiling: None,
            },
        )?
        .stdout,
    )
    .into_owned())
}

pub(crate) fn git_stdout(cwd: Option<&Path>, args: &[&str]) -> Result<String, SandboxError> {
    Ok(String::from_utf8_lossy(&git_run(cwd, args)?.stdout).into_owned())
}

pub(crate) fn git_bytes(cwd: Option<&Path>, args: &[&str]) -> Result<Vec<u8>, SandboxError> {
    git_run(cwd, args).map(|output| output.stdout)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// G1 local Git consumes an agent-writable repository that supplies a
    /// post-checkout hook and fsmonitor command -> builtin checkout/status still
    /// work, but neither program executes on the host. G2 a CLI-authored trusted
    /// operation keeps its working tree/ref behavior. This executes actual Git;
    /// an argv assertion alone would miss executable configuration consumers.
    #[test]
    fn local_repository_configuration_cannot_execute_host_hooks_or_monitors() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = tempfile::tempdir().unwrap();
        run_git(Some(root.path()), &["init", "-q"]).unwrap();
        run_git(
            Some(root.path()),
            &["config", "user.email", "test@example.invalid"],
        )
        .unwrap();
        run_git(Some(root.path()), &["config", "user.name", "fixture"]).unwrap();
        std::fs::write(root.path().join("source"), b"source").unwrap();
        run_git(Some(root.path()), &["add", "source"]).unwrap();
        run_git(Some(root.path()), &["commit", "-qm", "fixture"]).unwrap();
        let marker = root.path().join("host-program-ran");
        let hook = root.path().join(".git/hooks/post-checkout");
        std::fs::write(
            &hook,
            format!("#!/bin/sh\nprintf attacked > '{}'\n", marker.display()),
        )
        .unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
        run_git(
            Some(root.path()),
            &["config", "core.fsmonitor", hook.to_str().unwrap()],
        )
        .unwrap();
        run_git(Some(root.path()), &["checkout", "--detach", "HEAD"]).unwrap();
        run_git(Some(root.path()), &["status", "--porcelain"]).unwrap();
        assert!(!marker.exists(), "G1 untrusted host program withheld");
        assert_eq!(
            std::fs::read(root.path().join("source")).unwrap(),
            b"source",
            "G2"
        );
    }

    #[test]
    fn every_git_invocation_disables_detached_maintenance_in_both_branches() {
        // Invariant: an effect spawns no process that outlives it. R1 Local
        // branch with a working directory -> the composed argv carries every
        // maintenance-disabling `-c` before the caller's arguments; R2 Network
        // branch (anonymous HTTPS) -> the same options precede the transport
        // configuration and the caller's arguments. Both branches share one
        // composition, so the assertion is on the exact argv, not on a flag.
        let expected: Vec<&str> = GIT_HOST_PROCESS_FLOOR.to_vec();
        assert_eq!(
            expected,
            [
                "-c",
                "gc.auto=0",
                "-c",
                "gc.autoDetach=false",
                "-c",
                "maintenance.auto=false",
                "-c",
                "fetch.writeCommitGraph=false",
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "core.fsmonitor=false",
                "-c",
                "submodule.recurse=false",
            ],
            "the pinned option set is the invariant, not a convenience"
        );
        let cwd = tempfile::tempdir().unwrap();
        let caller_args = ["fetch", "--prune", "origin"];
        let endpoint = "https://example.invalid/repo";
        let branches = [
            (
                "R1 Local",
                compose_git_command(Some(cwd.path()), &caller_args, GitAuthentication::Local)
                    .unwrap(),
            ),
            (
                "R2 Network",
                compose_git_command(
                    None,
                    &caller_args,
                    GitAuthentication::Network {
                        credential: None,
                        transport: validate_credentialed_git_transport(endpoint, None).unwrap(),
                        alternate_objects: None,
                        discovery_ceiling: None,
                    },
                )
                .unwrap(),
            ),
        ];
        for (rule, (command, isolated_cwd)) in &branches {
            let argv: Vec<String> = command
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                &argv[..expected.len()],
                expected.as_slice(),
                "{rule}: maintenance is disabled before every other option: {argv:?}"
            );
            assert!(
                argv.ends_with(&[
                    "fetch".to_owned(),
                    "--prune".to_owned(),
                    "origin".to_owned()
                ]),
                "{rule}: caller arguments stay last: {argv:?}"
            );
            assert_eq!(
                argv.iter()
                    .filter(|arg| arg.as_str() == "gc.auto=0")
                    .count(),
                1,
                "{rule}: the option set is composed exactly once"
            );
            assert_eq!(
                isolated_cwd.is_some(),
                *rule == "R2 Network",
                "{rule}: only the Network branch owns an isolated working directory"
            );
        }
        let local_program = branches[0].1.0.get_program().to_string_lossy().into_owned();
        assert!(Path::new(&local_program).is_absolute());
    }

    #[test]
    fn failed_git_stderr_redacts_command_credentials_at_the_canonical_seam() {
        // R1 failed credentialed child echoes username/password -> retain
        // diagnostic context, redact both exact values before any caller/codec.
        // R2 anonymous failure -> retain ordinary stderr. Real Git alias emits
        // controlled bytes; no global env mutation, remote server or real secret.
        let credential = awaken_provisioning_contract::RepositoryHttpBasicCredential::new(
            "unique-http-user".to_owned(),
            "unique-http-password".to_owned(),
        );
        let endpoint = "https://example.invalid/repo";
        for material in [Some(&credential), None] {
            let error = git_command_output(
                None,
                &["-c",
                  r#"alias.fixture=!printf 'failure-context %s %s' "$AWAKEN_GIT_CREDENTIAL_USERNAME" "$AWAKEN_GIT_CREDENTIAL_PASSWORD" >&2; exit 1"#,
                  "fixture"],
                GitAuthentication::Network {
                    credential: material,
                    transport: validate_credentialed_git_transport(endpoint, material).unwrap(),
                    alternate_objects: None,
                discovery_ceiling: None,
                },
            ).unwrap_err().to_string();
            assert!(error.contains("failure-context"));
            assert!(!error.contains(credential.expose_username()));
            assert!(!error.contains(credential.expose_password()));
            assert_eq!(
                error.matches("[REDACTED]").count(),
                if material.is_some() { 2 } else { 0 }
            );
        }
    }
}

#[cfg(test)]
mod repository_scope_tests {
    use super::*;

    #[test]
    fn credentialed_clone_uses_the_owned_stage_for_relative_roots() {
        let source = tempfile::tempdir().unwrap();
        run_git(Some(source.path()), &["init", "-q"]).unwrap();
        std::fs::write(source.path().join("a.txt"), b"fixture").unwrap();
        run_git(Some(source.path()), &["add", "a.txt"]).unwrap();
        run_git(
            Some(source.path()),
            &[
                "-c",
                "user.name=fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        )
        .unwrap();
        let url = url::Url::from_directory_path(source.path())
            .unwrap()
            .to_string();
        // Keep process cwd unchanged. A caller's relative root must still
        // publish here, while network Git runs from a separate isolated cwd.
        let directory = tempfile::Builder::new()
            .prefix(".awaken-clone-")
            .tempdir_in(".")
            .unwrap();
        let cwd = std::env::current_dir().unwrap();
        let relative_root = Path::new(directory.path().file_name().unwrap()).join("jail");
        assert!(relative_root.is_relative());
        crate::git_transport::provision_repo_at(
            &crate::IsolatedRoot::new(relative_root),
            "workspace/repo",
            &url,
            None,
            None,
            None,
            awaken_provisioning_contract::RepositoryRealizationIntent::Initialize,
        )
        .unwrap();
        let repository = directory.path().join("jail/workspace/repo");
        assert_eq!(std::fs::read(repository.join("a.txt")).unwrap(), b"fixture");
        assert_eq!(
            git_stdout(Some(&repository), &["remote", "get-url", "origin"])
                .unwrap()
                .trim(),
            url
        );
        assert_eq!(std::env::current_dir().unwrap(), cwd);
        directory.close().unwrap();
    }

    #[test]
    fn local_repository_commands_never_mutate_an_ancestor_checkout() {
        // R1 an expected repository is missing beneath a real ancestor repo;
        // R2 an isolated -C invocation selects the same missing repo. Both
        // reject, rather than reading/writing the ancestor's origin config.
        let ancestor = tempfile::tempdir().unwrap();
        run_git(Some(ancestor.path()), &["init", "-q"]).unwrap();
        let origin = "https://original.invalid/repository";
        run_git(Some(ancestor.path()), &["remote", "add", "origin", origin]).unwrap();
        let child = ancestor.path().join("missing-repository");
        std::fs::create_dir(&child).unwrap();
        assert!(git_stdout(Some(&child), &["rev-parse", "--show-toplevel"]).is_err());
        assert!(
            run_git(
                Some(&child),
                &["remote", "set-url", "origin", "https://wrong.invalid/repo"]
            )
            .is_err()
        );
        assert!(
            git_command_output(
                None,
                &[
                    "-C",
                    child.to_str().unwrap(),
                    "remote",
                    "set-url",
                    "origin",
                    "https://wrong.invalid/repo"
                ],
                GitAuthentication::Network {
                    credential: None,
                    transport: AdmittedGitTransport {
                        protocol: "file".into(),
                        credential_endpoint: None
                    },
                    alternate_objects: None,
                    discovery_ceiling: child.parent(),
                },
            )
            .is_err()
        );
        assert_eq!(
            git_stdout(Some(ancestor.path()), &["remote", "get-url", "origin"])
                .unwrap()
                .trim(),
            origin
        );
    }
}
