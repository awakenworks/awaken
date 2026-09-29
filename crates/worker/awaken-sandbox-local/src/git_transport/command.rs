// @formal surface requirement=credential.plaintext_boundary.exact_holder
// @formal reason: Static owner: the one private Git process constructor and command-scoped HTTP Basic
// @formal reason: credential holder beneath the local Repository transport. Dynamic path: network commands
// @formal reason: admit the exact protocol and authority and remove ambient Git, SSH, proxy, TLS, trace,
// @formal reason: and credential configuration; local commands remove explicit Repository-selection
// @formal reason: variables plus Awaken and askpass credential variables, use the caller-provided working
// @formal reason: directory when supplied, and otherwise execute the caller-selected argument-only local
// @formal reason: operation. Credentials are released only through the exact protocol-plus-authority
// @formal reason: helper, and this module selects no clone, push, ref, CAS outcome, or receipt. Evidence:
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
//! argument-only local operation. It owns no clone or push policy, remote-write
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
    let parsed = match url::Url::parse(url) {
        Ok(parsed) => parsed,
        Err(_)
            if credential.is_none()
                && cfg!(any(test, feature = "test-support"))
                && Path::new(url).is_absolute() =>
        {
            return Ok(AdmittedGitTransport {
                protocol: "file".into(),
                credential_endpoint: None,
            });
        }
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
    command
        .args(GIT_HOST_PROCESS_FLOOR)
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
        .env_remove("GIT_NAMESPACE");
    let mut isolated_cwd = None;
    if let GitAuthentication::Network {
        credential,
        transport,
        alternate_objects,
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
    } else if let Some(dir) = cwd {
        command.current_dir(dir);
    }
    command.args(args);
    Ok((command, isolated_cwd))
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
