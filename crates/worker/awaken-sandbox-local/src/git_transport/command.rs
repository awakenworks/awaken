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
            return Err(SandboxError(
                "repository transport must be an absolute HTTPS endpoint".into(),
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
            return Err(SandboxError(
                "anonymous repository transport must be HTTPS without embedded user info".into(),
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
        return Err(SandboxError(
            "credentialed repository URL must use its admitted HTTP transport without embedded user info".into(),
        ));
    }
    if !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.host().is_none()
        || url.bytes().any(|byte| byte.is_ascii_whitespace())
    {
        return Err(SandboxError(
            "credentialed repository URL must use its admitted HTTP transport without embedded user info".into(),
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
        return Err(SandboxError(
            "repository HTTP Basic credentials contain an invalid protocol value".into(),
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

const GIT_CREDENTIAL_USERNAME_ENV: &str = "AWAKEN_GIT_CREDENTIAL_USERNAME";
const GIT_CREDENTIAL_PASSWORD_ENV: &str = "AWAKEN_GIT_CREDENTIAL_PASSWORD";
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

fn git_command_output(
    cwd: Option<&Path>,
    args: &[&str],
    authentication: GitAuthentication<'_>,
) -> Result<std::process::Output, SandboxError> {
    let mut command = std::process::Command::new("git");
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
        .env_remove("GIT_NAMESPACE");
    let isolated_cwd;
    if let GitAuthentication::Network {
        credential,
        transport,
        alternate_objects,
    } = authentication
    {
        isolated_cwd = tempfile::tempdir().map_err(|error| SandboxError(error.to_string()))?;
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
                "core.hooksPath=/dev/null",
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
                SandboxError("repository object path cannot form a Git alternate".into())
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
    let output = command
        .output()
        .map_err(|error| SandboxError(format!("git {}: {error}", args.first().unwrap_or(&""))))?;
    if !output.status.success() {
        return Err(SandboxError(format!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
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
