//! Exact, ambient-free host Git transport for Repository realization.

use std::path::{Path, PathBuf};

use crate::{IsolatedRoot, SandboxError, jailed_at};

/// Clone a git repository into `<root>/<logical>` **host-side** (ADR-0038). The
/// git process is the only credential holder: one command-scoped credential
/// helper supplies Basic auth while `clone` receives and persists only the clean
/// URL. Fail-closed: a bad `logical` or non-zero git exit removes only the
/// newly-created validated destination, so neither a partial target nor its Git
/// config survives.
pub(crate) fn provision_repo_at(
    root: &IsolatedRoot,
    logical: &str,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<(), SandboxError> {
    let dest = jailed_at(root, logical)?;
    match std::fs::symlink_metadata(&dest) {
        Ok(_) if realized_repository_matches(&dest, url, initial_branch, initial_commit) => {
            return Ok(());
        }
        Ok(_) => {
            return Err(SandboxError(format!(
                "repository destination `{}` already exists with a different realization",
                dest.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(SandboxError(error.to_string())),
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|error| SandboxError(error.to_string()))?;
    }
    let mut args = vec!["clone".to_string(), "--no-checkout".to_string()];
    if let Some(branch) = initial_branch {
        args.push("--branch".into());
        args.push(branch.to_string());
    }
    args.push("--".into());
    args.push(url.to_string());
    args.push(dest.to_string_lossy().into_owned());
    let realization = (|| {
        credentialed_git_run(url, &args, credential)?;
        if let Some(commit) = initial_commit {
            run_git(Some(&dest), &["checkout", "--detach", commit])?;
        } else {
            // Populate the selected/default branch only after the credentialed
            // process has exited. Repository filters and checkout behavior never
            // inherit the operation credential.
            run_git(Some(&dest), &["reset", "--hard", "HEAD"])?;
        }
        Ok(())
    })();
    if let Err(error) = realization {
        match std::fs::remove_dir_all(&dest) {
            Ok(()) => return Err(error),
            Err(cleanup) if cleanup.kind() == std::io::ErrorKind::NotFound => return Err(error),
            Err(cleanup) => {
                return Err(SandboxError(format!(
                    "{error}; failed to remove partial repository `{}`: {cleanup}",
                    dest.display()
                )));
            }
        }
    }
    Ok(())
}

fn realized_repository_matches(
    destination: &Path,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
) -> bool {
    if !git_stdout(Some(destination), &["remote", "get-url", "origin"])
        .is_ok_and(|actual| actual.trim() == url)
    {
        return false;
    }
    if initial_branch.is_some_and(|expected| {
        !git_stdout(Some(destination), &["symbolic-ref", "--short", "HEAD"])
            .is_ok_and(|actual| actual.trim() == expected)
    }) {
        return false;
    }
    if let Some(expected) = initial_commit {
        let Ok(actual) = git_stdout(Some(destination), &["rev-parse", "HEAD"]) else {
            return false;
        };
        let Ok(expected) = git_stdout(Some(destination), &["rev-parse", expected]) else {
            return false;
        };
        if actual.trim() != expected.trim() {
            return false;
        }
    }
    true
}

struct RepositoryPublishSource {
    branch: String,
    commit: String,
    objects: PathBuf,
}

fn repository_publish_source(
    root: &IsolatedRoot,
    destination: &Path,
) -> Result<RepositoryPublishSource, SandboxError> {
    let destination_metadata =
        std::fs::symlink_metadata(destination).map_err(|error| SandboxError(error.to_string()))?;
    if destination_metadata.file_type().is_symlink() || !destination_metadata.is_dir() {
        return Err(SandboxError(
            "repository destination is not an in-jail directory".into(),
        ));
    }
    let canonical_root = root
        .root()
        .canonicalize()
        .map_err(|error| SandboxError(error.to_string()))?;
    let canonical_destination = destination
        .canonicalize()
        .map_err(|error| SandboxError(error.to_string()))?;
    if !canonical_destination.starts_with(&canonical_root) {
        return Err(SandboxError(
            "repository destination escapes the sandbox root".into(),
        ));
    }
    let git_dir = destination.join(".git");
    let git_metadata =
        std::fs::symlink_metadata(&git_dir).map_err(|error| SandboxError(error.to_string()))?;
    if git_metadata.file_type().is_symlink() || !git_metadata.is_dir() {
        return Err(SandboxError(
            "repository Git directory is not an in-tree directory".into(),
        ));
    }
    let git_dir = git_dir
        .canonicalize()
        .map_err(|error| SandboxError(error.to_string()))?;
    let objects = git_dir.join("objects");
    let object_metadata =
        std::fs::symlink_metadata(&objects).map_err(|error| SandboxError(error.to_string()))?;
    if object_metadata.file_type().is_symlink() || !object_metadata.is_dir() {
        return Err(SandboxError(
            "repository object directory is not an in-tree directory".into(),
        ));
    }
    let objects = objects
        .canonicalize()
        .map_err(|error| SandboxError(error.to_string()))?;
    if !git_dir.starts_with(&canonical_destination) || !objects.starts_with(&git_dir) {
        return Err(SandboxError(
            "repository Git object directory escapes the working tree".into(),
        ));
    }
    for alternate in ["info/alternates", "info/http-alternates"] {
        if std::fs::symlink_metadata(objects.join(alternate)).is_ok() {
            return Err(SandboxError(
                "repository object alternates are not publishable".into(),
            ));
        }
    }

    let branch = git_stdout(
        Some(destination),
        &["symbolic-ref", "--quiet", "--short", "HEAD"],
    )
    .map_err(|_| SandboxError("cannot push a repository with detached HEAD".into()))?;
    let branch = branch.trim();
    if branch.is_empty() || branch == "HEAD" {
        return Err(SandboxError(
            "cannot push a repository with detached HEAD".into(),
        ));
    }
    run_git(None, &["check-ref-format", "--branch", branch])?;
    let commit = git_stdout(
        Some(destination),
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?;
    let commit = commit.trim();
    if !matches!(commit.len(), 40 | 64) || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SandboxError(
            "repository HEAD did not resolve to an object identifier".into(),
        ));
    }

    Ok(RepositoryPublishSource {
        branch: branch.to_owned(),
        commit: commit.to_owned(),
        objects,
    })
}

fn ephemeral_publish_git_dir(commit: &str) -> Result<tempfile::TempDir, SandboxError> {
    let temp = tempfile::tempdir().map_err(|error| SandboxError(error.to_string()))?;
    let git_dir = temp.path().join("git");
    for directory in [
        git_dir.join("objects/info"),
        git_dir.join("objects/pack"),
        git_dir.join("refs/heads"),
        git_dir.join("refs/tags"),
    ] {
        std::fs::create_dir_all(directory).map_err(|error| SandboxError(error.to_string()))?;
    }
    std::fs::write(git_dir.join("HEAD"), b"ref: refs/heads/awaken-publish\n")
        .map_err(|error| SandboxError(error.to_string()))?;
    std::fs::write(
        git_dir.join("refs/heads/awaken-publish"),
        format!("{commit}\n"),
    )
    .map_err(|error| SandboxError(error.to_string()))?;
    Ok(temp)
}

/// Publish the Agent-authored current branch to the exact host-frozen remote.
/// Agent-writable origin, URL rewrite, header, credential, and hook configuration
/// is never consulted by a network Git process. The push runs from a temporary,
/// config-free bare Git directory whose sole source ref names the exact observed
/// commit and whose read-only object alternate is the in-tree clone object store.
pub(crate) fn push_repo_to_at(
    root: &IsolatedRoot,
    logical: &str,
    remote_url: &str,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<bool, SandboxError> {
    let dest = jailed_at(root, logical)?;
    let source = repository_publish_source(root, &dest)?;
    let remote_ref = format!("refs/heads/{}", source.branch);
    let remote = credentialed_git_stdout(
        remote_url,
        &["ls-remote", "--", remote_url, &remote_ref],
        credential,
    )?;
    if remote.split_whitespace().next() == Some(source.commit.as_str()) {
        return Ok(false);
    }

    let clean_git = ephemeral_publish_git_dir(&source.commit)?;
    let git_dir = clean_git.path().join("git");
    let git_dir_arg = format!("--git-dir={}", git_dir.display());
    let refspec = format!("refs/heads/awaken-publish:refs/heads/{}", source.branch);
    credentialed_git_run_with_alternate(
        remote_url,
        &[
            git_dir_arg,
            "push".into(),
            "--".into(),
            remote_url.into(),
            refspec,
        ],
        credential,
        &source.objects,
    )?;
    Ok(true)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdmittedGitEndpoint {
    protocol: String,
    authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AdmittedGitTransport {
    protocol: String,
    credential_endpoint: Option<AdmittedGitEndpoint>,
}

fn validate_credentialed_git_transport(
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

fn credentialed_git_run(
    url: &str,
    args: &[impl AsRef<str>],
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<(), SandboxError> {
    credentialed_git_run_inner(url, args, credential, None)
}

fn credentialed_git_run_with_alternate(
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

fn credentialed_git_stdout(
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

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    fn read_http_request(stream: &mut std::net::TcpStream) -> (String, Vec<String>) {
        let mut request_line = String::new();
        let mut headers = Vec::new();
        let mut request = std::io::BufReader::new(stream);
        request.read_line(&mut request_line).unwrap();
        loop {
            let mut line = String::new();
            request.read_line(&mut line).unwrap();
            if line == "\r\n" || line.is_empty() {
                break;
            }
            headers.push(line);
        }
        (request_line, headers)
    }

    fn has_basic_authorization(headers: &[String]) -> bool {
        headers.iter().any(|line| {
            line.to_ascii_lowercase()
                .starts_with("authorization: basic ")
        })
    }

    struct StaticGitHttp {
        url: String,
        saw_authorization: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl StaticGitHttp {
        fn serve(root: &Path, repository: &str) -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let root = root.to_path_buf();
            let saw_authorization = Arc::new(AtomicBool::new(false));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread = {
                let saw_authorization = saw_authorization.clone();
                let shutdown = shutdown.clone();
                std::thread::spawn(move || {
                    while !shutdown.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                let (request_line, headers) = read_http_request(&mut stream);
                                if !has_basic_authorization(&headers) {
                                    stream
                                        .write_all(
                                            b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=git\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                        )
                                        .unwrap();
                                    continue;
                                }
                                saw_authorization.store(true, Ordering::SeqCst);
                                let mut parts = request_line.split_whitespace();
                                let method = parts.next().unwrap_or_default();
                                let requested = parts
                                    .next()
                                    .unwrap_or_default()
                                    .split('?')
                                    .next()
                                    .unwrap_or_default()
                                    .trim_start_matches('/');
                                let relative = Path::new(requested);
                                let safe = relative.components().all(|component| {
                                    matches!(component, std::path::Component::Normal(_))
                                });
                                let bytes = safe
                                    .then(|| std::fs::read(root.join(relative)).ok())
                                    .flatten();
                                match bytes {
                                    Some(bytes) => {
                                        let content_type = if requested.ends_with("info/refs") {
                                            "text/plain"
                                        } else {
                                            "application/octet-stream"
                                        };
                                        write!(
                                            stream,
                                            "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                            bytes.len()
                                        )
                                        .unwrap();
                                        if method != "HEAD" {
                                            stream.write_all(&bytes).unwrap();
                                        }
                                    }
                                    None => stream
                                        .write_all(
                                            b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                        )
                                        .unwrap(),
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Err(error) => panic!("static Git HTTP accept failed: {error}"),
                        }
                    }
                })
            };
            Self {
                url: format!("http://{address}/{repository}"),
                saw_authorization,
                shutdown,
                thread: Some(thread),
            }
        }
    }

    impl Drop for StaticGitHttp {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct AmbientProxyProbe {
        url: String,
        saw_connection: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
        thread: Option<std::thread::JoinHandle<()>>,
    }

    impl AmbientProxyProbe {
        fn serve() -> Self {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.set_nonblocking(true).unwrap();
            let address = listener.local_addr().unwrap();
            let saw_connection = Arc::new(AtomicBool::new(false));
            let shutdown = Arc::new(AtomicBool::new(false));
            let thread = {
                let saw_connection = saw_connection.clone();
                let shutdown = shutdown.clone();
                std::thread::spawn(move || {
                    while !shutdown.load(Ordering::SeqCst) {
                        match listener.accept() {
                            Ok((mut stream, _)) => {
                                saw_connection.store(true, Ordering::SeqCst);
                                let _ = stream.write_all(
                                    b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                );
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Err(error) => panic!("ambient proxy accept failed: {error}"),
                        }
                    }
                })
            };
            Self {
                url: format!("http://{address}"),
                saw_connection,
                shutdown,
                thread: Some(thread),
            }
        }
    }

    impl Drop for AmbientProxyProbe {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            if let Some(thread) = self.thread.take() {
                let _ = thread.join();
            }
        }
    }

    struct CrossHostRedirectGitHttp {
        url: String,
        source_saw_authorization: Arc<AtomicBool>,
        attacker_saw_request: Arc<AtomicBool>,
        attacker_saw_authorization: Arc<AtomicBool>,
        shutdown: Arc<AtomicBool>,
        threads: Vec<std::thread::JoinHandle<()>>,
    }

    impl CrossHostRedirectGitHttp {
        fn serve() -> Self {
            let attacker = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            attacker.set_nonblocking(true).unwrap();
            let attacker_address = attacker.local_addr().unwrap();
            let source = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            source.set_nonblocking(true).unwrap();
            let source_address = source.local_addr().unwrap();
            let source_saw_authorization = Arc::new(AtomicBool::new(false));
            let attacker_saw_request = Arc::new(AtomicBool::new(false));
            let attacker_saw_authorization = Arc::new(AtomicBool::new(false));
            let shutdown = Arc::new(AtomicBool::new(false));

            let source_thread = {
                let source_saw_authorization = source_saw_authorization.clone();
                let shutdown = shutdown.clone();
                std::thread::spawn(move || {
                    while !shutdown.load(Ordering::SeqCst) {
                        match source.accept() {
                            Ok((mut stream, _)) => {
                                let (request_line, headers) = read_http_request(&mut stream);
                                if has_basic_authorization(&headers) {
                                    source_saw_authorization.store(true, Ordering::SeqCst);
                                    let path_and_query =
                                        request_line.split_whitespace().nth(1).unwrap_or(
                                            "/repository.git/info/refs?service=git-upload-pack",
                                        );
                                    let response = format!(
                                        "HTTP/1.1 302 Found\r\nLocation: http://{attacker_address}{path_and_query}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                    );
                                    let _ = stream.write_all(response.as_bytes());
                                } else {
                                    let _ = stream.write_all(
                                        b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=git\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                    );
                                }
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Err(error) => panic!("redirect source accept failed: {error}"),
                        }
                    }
                })
            };
            let attacker_thread = {
                let attacker_saw_request = attacker_saw_request.clone();
                let attacker_saw_authorization = attacker_saw_authorization.clone();
                let shutdown = shutdown.clone();
                std::thread::spawn(move || {
                    while !shutdown.load(Ordering::SeqCst) {
                        match attacker.accept() {
                            Ok((mut stream, _)) => {
                                let (_, headers) = read_http_request(&mut stream);
                                attacker_saw_request.store(true, Ordering::SeqCst);
                                if has_basic_authorization(&headers) {
                                    attacker_saw_authorization.store(true, Ordering::SeqCst);
                                }
                                let _ = stream.write_all(
                                    b"HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Basic realm=attacker\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                                );
                            }
                            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                                std::thread::sleep(std::time::Duration::from_millis(2));
                            }
                            Err(error) => panic!("redirect attacker accept failed: {error}"),
                        }
                    }
                })
            };

            Self {
                url: format!("http://localhost:{}/repository.git", source_address.port()),
                source_saw_authorization,
                attacker_saw_request,
                attacker_saw_authorization,
                shutdown,
                threads: vec![source_thread, attacker_thread],
            }
        }
    }

    impl Drop for CrossHostRedirectGitHttp {
        fn drop(&mut self) {
            self.shutdown.store(true, Ordering::SeqCst);
            for thread in self.threads.drain(..) {
                let _ = thread.join();
            }
        }
    }

    #[test]
    fn repository_basic_auth_command_follows_the_transport_decision_table() {
        // Cause/effect rules: anonymous HTTPS is admitted; anonymous HTTP/SSH
        // rejects; local is unit-fixture-only; upstream Basic is HTTPS-only;
        // Gateway Basic admits HTTP(S); userinfo/control-bearing fields reject.
        let credential = awaken_provisioning_contract::RepositoryHttpBasicCredential::new(
            "git user".to_string(),
            "p@ss".to_string(),
        );
        assert!(
            validate_credentialed_git_transport("https://example.test/repo.git", None).is_ok(),
            "R1"
        );
        assert!(
            validate_credentialed_git_transport("file:///repo", None).is_ok(),
            "R2"
        );
        assert!(
            validate_credentialed_git_transport("http://example.test/repo.git", None).is_err(),
            "R3/http"
        );
        assert!(
            validate_credentialed_git_transport("ssh://example.test/repo.git", None).is_err(),
            "R3/ssh"
        );
        assert!(
            validate_credentialed_git_transport("https://example.test/repo.git", Some(&credential))
                .is_ok(),
            "R4"
        );
        assert!(
            validate_credentialed_git_transport("http://example.test/repo.git", Some(&credential))
                .is_err(),
            "R5"
        );
        assert!(
            validate_credentialed_git_transport(
                "https://already@example.test/repo.git",
                Some(&credential)
            )
            .is_err(),
            "R6"
        );
        let gateway =
            awaken_provisioning_contract::RepositoryHttpBasicCredential::gateway_capability(
                "short-lived-capability".to_owned(),
            );
        assert!(
            validate_credentialed_git_transport("http://gateway.internal/repo.git", Some(&gateway))
                .is_ok(),
            "R7"
        );
        assert!(
            validate_credentialed_git_transport(
                "https://gateway.internal/repo.git",
                Some(&gateway)
            )
            .is_ok(),
            "R8"
        );
        assert!(
            validate_credentialed_git_transport("ssh://gateway.internal/repo.git", Some(&gateway))
                .is_err(),
            "R9"
        );
        let control = awaken_provisioning_contract::RepositoryHttpBasicCredential::new(
            "x-access-token".to_string(),
            "line-one\nline-two".to_string(),
        );
        assert!(
            validate_credentialed_git_transport("https://example.test/repo.git", Some(&control))
                .is_err(),
            "R10"
        );
    }

    /// Ambient Git process cause/effect decision table:
    /// A1 a test-only local target plus hostile HOME rewrite/fake SSH/netrc uses
    /// the exact local target and no ambient auth; A2 anonymous SSH rejects
    /// before Git starts; A3 inherited Git/cURL TLS overrides are absent from
    /// the network child, so the target's certificate verification cannot be
    /// downgraded through parent state; A4 inherited trace/redaction/redirect
    /// controls create no persistent trace and no credential-bearing error; A5
    /// inherited lower/upper-case proxy controls are absent, so a credentialed
    /// exact target connects directly and the attacker proxy sees no connection.
    #[cfg(unix)]
    #[test]
    fn anonymous_git_transport_does_not_consume_ambient_authentication() {
        use std::os::unix::fs::PermissionsExt as _;

        let temp = tempfile::tempdir().unwrap();
        let repository = temp.path().join("repository.git");
        run_git(None, &["init", "--bare", repository.to_str().unwrap()]).unwrap();
        let hostile_home = temp.path().join("hostile-home");
        std::fs::create_dir_all(&hostile_home).unwrap();
        std::fs::write(
            hostile_home.join(".gitconfig"),
            format!(
                "[url \"ssh://attacker.invalid/\"]\n\tinsteadOf = {}\n",
                repository.display()
            ),
        )
        .unwrap();
        std::fs::write(
            hostile_home.join(".netrc"),
            "machine attacker.invalid login ambient password ambient-secret\n",
        )
        .unwrap();
        let marker = temp.path().join("ambient-ssh-used");
        let git_trace = temp.path().join("ambient-git-trace");
        let git_trace2 = temp.path().join("ambient-git-trace2");
        let redirected_stderr = temp.path().join("ambient-git-stderr");
        let hostile_ca = temp.path().join("ambient-ca.pem");
        let hostile_ca_dir = temp.path().join("ambient-ca-dir");
        let hostile_exec_path = temp.path().join("ambient-git-exec");
        let hostile_template_dir = temp.path().join("ambient-git-template");
        let attacker_proxy = AmbientProxyProbe::serve();
        let fake_ssh = temp.path().join("fake-ssh");
        std::fs::write(
            &fake_ssh,
            "#!/bin/sh\n: > \"$AWAKEN_AMBIENT_SSH_MARKER\"\nexit 97\n",
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&fake_ssh).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&fake_ssh, permissions).unwrap();

        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "git_transport::tests::anonymous_git_transport_hostile_child",
                "--ignored",
                "--nocapture",
            ])
            .env("HOME", &hostile_home)
            .env("XDG_CONFIG_HOME", &hostile_home)
            .env("GIT_SSH_COMMAND", &fake_ssh)
            .env("SSH_AUTH_SOCK", temp.path().join("fake-agent.sock"))
            .env("GIT_SSL_NO_VERIFY", "1")
            .env("GIT_SSL_CAINFO", &hostile_ca)
            .env("GIT_SSL_CAPATH", &hostile_ca_dir)
            .env("GIT_SSL_VERSION", "sslv3")
            .env("GIT_SSL_CIPHER_LIST", "AMBIENT-CIPHER")
            .env("GIT_PROXY_SSL_CAINFO", &hostile_ca)
            .env("CURL_CA_BUNDLE", &hostile_ca)
            .env("SSL_CERT_FILE", &hostile_ca)
            .env("SSL_CERT_DIR", &hostile_ca_dir)
            .env("GIT_TRACE", &git_trace)
            .env("GIT_TRACE_CURL", &git_trace)
            .env("GIT_TRACE2_EVENT", &git_trace2)
            .env("GIT_TRACE_REDACT", "0")
            .env("GIT_CURL_VERBOSE", "1")
            .env("GIT_REDIRECT_STDERR", &redirected_stderr)
            .env("GIT_EXEC_PATH", &hostile_exec_path)
            .env("GIT_TEMPLATE_DIR", &hostile_template_dir)
            .env("http_proxy", &attacker_proxy.url)
            .env("https_proxy", &attacker_proxy.url)
            .env("all_proxy", &attacker_proxy.url)
            .env("no_proxy", "")
            .env("HTTP_PROXY", &attacker_proxy.url)
            .env("HTTPS_PROXY", &attacker_proxy.url)
            .env("ALL_PROXY", &attacker_proxy.url)
            .env("NO_PROXY", "")
            .env("AWAKEN_AMBIENT_GIT_EXEC_PATH", &hostile_exec_path)
            .env("AWAKEN_AMBIENT_GIT_TEMPLATE_DIR", &hostile_template_dir)
            .env("AWAKEN_AMBIENT_SSH_MARKER", &marker)
            .env("AWAKEN_AMBIENT_LOCAL_REPOSITORY", &repository)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "child failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!marker.exists(), "A1+A2 ambient SSH must never execute");
        assert!(!git_trace.exists(), "A4 must not create a Git trace");
        assert!(!git_trace2.exists(), "A4 must not create a trace2 log");
        assert!(
            !redirected_stderr.exists(),
            "A4 must not redirect Git stderr to a persistent path"
        );
        assert!(
            !attacker_proxy.saw_connection.load(Ordering::SeqCst),
            "A5 an ambient proxy must see zero network Git connections"
        );
    }

    #[cfg(unix)]
    #[test]
    #[ignore = "spawned by anonymous_git_transport_does_not_consume_ambient_authentication"]
    fn anonymous_git_transport_hostile_child() {
        let repository = std::env::var("AWAKEN_AMBIENT_LOCAL_REPOSITORY").unwrap();
        let effective_environment = credentialed_git_stdout(
            "https://example.invalid/repository.git",
            &["-c", "alias.awaken-env=!env", "awaken-env"],
            None,
        )
        .expect("A3 inspect the isolated network child environment");
        let forbidden_prefixes = [
            "GIT_SSL_",
            "GIT_PROXY_SSL_",
            "GIT_TRACE",
            "GIT_CURL_VERBOSE=",
            "GIT_REDIRECT_",
            "CURL_CA_BUNDLE=",
            "SSL_CERT_FILE=",
            "SSL_CERT_DIR=",
            "http_proxy=",
            "https_proxy=",
            "all_proxy=",
            "no_proxy=",
            "HTTP_PROXY=",
            "HTTPS_PROXY=",
            "ALL_PROXY=",
            "NO_PROXY=",
        ];
        assert!(
            effective_environment.lines().all(|line| {
                forbidden_prefixes
                    .iter()
                    .all(|prefix| !line.starts_with(prefix))
            }),
            "A3+A4+A5 isolated Git must not inherit TLS, trace, redirect, CA, or proxy overrides"
        );
        for (key, hostile_value) in [
            (
                "GIT_EXEC_PATH=",
                std::env::var("AWAKEN_AMBIENT_GIT_EXEC_PATH").unwrap(),
            ),
            (
                "GIT_TEMPLATE_DIR=",
                std::env::var("AWAKEN_AMBIENT_GIT_TEMPLATE_DIR").unwrap(),
            ),
        ] {
            assert!(
                effective_environment
                    .lines()
                    .all(|line| line.strip_prefix(key) != Some(hostile_value.as_str())),
                "A3+A4 Git may derive a trusted built-in path but must not inherit the hostile parent value"
            );
        }
        credentialed_git_stdout(
            &repository,
            &["ls-remote", "--", &repository, "refs/heads/main"],
            None,
        )
        .expect("A1 exact local fixture ignores hostile HOME/global config");
        let error = credentialed_git_stdout(
            "ssh://attacker.invalid/repository.git",
            &[
                "ls-remote",
                "--",
                "ssh://attacker.invalid/repository.git",
                "refs/heads/main",
            ],
            None,
        )
        .expect_err("A2 SSH has no typed credential path");
        assert!(
            error
                .0
                .contains("anonymous repository transport must be HTTPS")
        );

        let server = CrossHostRedirectGitHttp::serve();
        let credential =
            awaken_provisioning_contract::RepositoryHttpBasicCredential::gateway_capability(
                "ambient-trace-capability".to_owned(),
            );
        let error = credentialed_git_stdout(
            &server.url,
            &["ls-remote", "--", &server.url, "refs/heads/main"],
            Some(&credential),
        )
        .expect_err("A4 the redirect remains unauthorized");
        for secret_representation in [
            "ambient-trace-capability",
            "Z2l0OmFtYmllbnQtdHJhY2UtY2FwYWJpbGl0eQ==",
        ] {
            assert!(
                !error.0.contains(secret_representation),
                "A4 Git errors must not contain a raw or encoded credential"
            );
        }
    }

    /// H1 admitted origin receives Basic; H2 a cross-host redirect retaining
    /// the Git service query receives no Authorization; H3 its challenge cannot
    /// obtain a credential from the exact protocol+host:port helper.
    #[test]
    fn credential_helper_never_authorizes_a_cross_host_redirect() {
        let server = CrossHostRedirectGitHttp::serve();
        let credential =
            awaken_provisioning_contract::RepositoryHttpBasicCredential::gateway_capability(
                "redirect-bound-capability".to_owned(),
            );
        let error = credentialed_git_stdout(
            &server.url,
            &["ls-remote", "--", &server.url, "refs/heads/main"],
            Some(&credential),
        )
        .expect_err("H3 redirected target has no admitted credential");
        assert!(
            server.source_saw_authorization.load(Ordering::SeqCst),
            "H1: {error}"
        );
        assert!(
            server.attacker_saw_request.load(Ordering::SeqCst),
            "H2: {error}"
        );
        assert!(
            !server.attacker_saw_authorization.load(Ordering::SeqCst),
            "H2+H3"
        );
    }

    /// Credentialed clone lifecycle: C1 authenticated exact checkout persists a
    /// clean origin; C2 authenticated clone plus invalid checkout removes the
    /// whole newly-created target and leaves no config/capability.
    #[test]
    fn credentialed_checkout_failure_leaves_no_repository_or_git_secret() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        let seed = base.join("credential-seed");
        std::fs::create_dir_all(&seed).unwrap();
        run_git(Some(&seed), &["init", "-q"]).unwrap();
        run_git(Some(&seed), &["checkout", "-q", "-b", "main"]).unwrap();
        run_git(Some(&seed), &["config", "user.email", "seed@t"]).unwrap();
        run_git(Some(&seed), &["config", "user.name", "seed"]).unwrap();
        std::fs::write(seed.join("README.md"), "credential checkout").unwrap();
        run_git(Some(&seed), &["add", "-A"]).unwrap();
        run_git(Some(&seed), &["commit", "-q", "-m", "seed"]).unwrap();
        let head = git_stdout(Some(&seed), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let bare = base.join("credential-remote.git");
        run_git(
            Some(base),
            &[
                "clone",
                "-q",
                "--bare",
                seed.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        )
        .unwrap();
        run_git(
            Some(base),
            &["--git-dir", bare.to_str().unwrap(), "update-server-info"],
        )
        .unwrap();
        let server = StaticGitHttp::serve(base, "credential-remote.git");
        let capability = "checkout-failure-capability";
        let credential =
            awaken_provisioning_contract::RepositoryHttpBasicCredential::gateway_capability(
                capability.to_owned(),
            );
        let root = IsolatedRoot::new(base.join("jail"));

        provision_repo_at(
            &root,
            "workspace/exact",
            &server.url,
            None,
            Some(&head),
            Some(&credential),
        )
        .expect("C1 authenticated exact checkout");
        let exact_dir = root.root().join("workspace/exact");
        let origin = git_stdout(Some(&exact_dir), &["remote", "get-url", "origin"]).unwrap();
        assert_eq!(origin.trim(), server.url, "C1 clean persisted origin");
        assert!(
            !std::fs::read_to_string(exact_dir.join(".git/config"))
                .unwrap()
                .contains(capability),
            "C1 no capability in config"
        );
        let error = provision_repo_at(
            &root,
            "workspace/invalid",
            &server.url,
            None,
            Some("0000000000000000000000000000000000000000"),
            Some(&credential),
        )
        .expect_err("C2 invalid checkout must fail");
        assert!(error.to_string().contains("checkout"), "C2: {error}");
        assert!(
            !root.root().join("workspace/invalid").exists(),
            "C2 no partial target"
        );
        assert!(
            server.saw_authorization.load(Ordering::SeqCst),
            "C1+C2 authenticated"
        );
    }

    #[test]
    fn explicit_remote_push_rejects_a_detached_head_before_network_access() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        run_git(Some(tmp.path()), &["init", "repo"]).unwrap();
        std::fs::write(repo.join("README.md"), "seed").unwrap();
        run_git(
            Some(&repo),
            &[
                "-c",
                "user.name=Awaken Test",
                "-c",
                "user.email=test@awaken.local",
                "add",
                "README.md",
            ],
        )
        .unwrap();
        run_git(
            Some(&repo),
            &[
                "-c",
                "user.name=Awaken Test",
                "-c",
                "user.email=test@awaken.local",
                "commit",
                "-m",
                "seed",
            ],
        )
        .unwrap();
        run_git(Some(&repo), &["checkout", "--detach"]).unwrap();
        let root = IsolatedRoot::new(tmp.path());
        let error = push_repo_to_at(&root, "repo", "https://invalid.example/repo", None)
            .expect_err("detached HEAD rejects before network");
        assert!(error.0.contains("detached HEAD"));
    }
}
