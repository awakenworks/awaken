//! Credential-free Git bundle transfer across a container Session boundary.

use awaken_provisioning_contract as pc;
use tokio::io::AsyncReadExt;

const MAX_REPO_BUNDLE_BYTES: usize = 256 * 1024 * 1024;
static TRANSFER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) async fn provision(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    logical: &str,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
    token: Option<&str>,
) -> Result<(), pc::SandboxError> {
    let url_owned = url.to_string();
    let initial_branch_owned = initial_branch.map(str::to_string);
    let initial_commit_owned = initial_commit.map(str::to_string);
    let token_owned = token.map(str::to_string);
    let bundle = tokio::task::spawn_blocking(move || {
        awaken_sandbox_local::clone_repo_bundle(
            &url_owned,
            initial_branch_owned.as_deref(),
            initial_commit_owned.as_deref(),
            token_owned.as_deref(),
        )
    })
    .await
    .map_err(|error| pc::SandboxError::new(error.to_string()))??;
    if bundle.len() > MAX_REPO_BUNDLE_BYTES {
        return Err(pc::SandboxError::new("repository bundle exceeds limit"));
    }
    let sequence = TRANSFER_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let bundle_path = format!("/tmp/awaken-repo-{sequence}.bundle");
    super::container_files::write(sandbox, &bundle_path, &bundle).await?;
    let destination = super::container_files::logical_path(logical)?;
    let process = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                concat!(
                    "set -eu; bundle=$1; destination=$2; remote=$3; branch=$4; commit=$5; ",
                    "trap 'rm -f -- \"$bundle\"' EXIT; ",
                    "test ! -e \"$destination\"; ",
                    "mkdir -p -- \"$(dirname -- \"$destination\")\"; ",
                    "git clone -- \"$bundle\" \"$destination\"; ",
                    "if test -n \"$branch\"; then git -C \"$destination\" checkout \"$branch\"; fi; ",
                    "if test -n \"$commit\"; then git -C \"$destination\" checkout --detach \"$commit\"; fi; ",
                    "git -C \"$destination\" remote set-url origin \"$remote\""
                )
                .into(),
                "awaken-repo-import".into(),
                bundle_path,
                destination,
                url.to_string(),
                initial_branch.unwrap_or_default().to_string(),
                initial_commit.unwrap_or_default().to_string(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    let status = process.wait().await?;
    if status.code == Some(0) {
        Ok(())
    } else {
        Err(pc::SandboxError::new(format!(
            "container repository import exited {:?}",
            status.code
        )))
    }
}

pub(super) async fn push(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    logical: &str,
    url: &str,
    token: Option<&str>,
) -> Result<bool, pc::SandboxError> {
    let repo = super::container_files::logical_path(logical)?;
    let process = sandbox
        .spawn_agent_process(pc::Command {
            argv: vec![
                "git".into(),
                "-C".into(),
                repo,
                "bundle".into(),
                "create".into(),
                "-".into(),
                "--all".into(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await?;
    let mut bundle = Vec::new();
    process
        .channel
        .take((MAX_REPO_BUNDLE_BYTES + 1) as u64)
        .read_to_end(&mut bundle)
        .await
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    if bundle.len() > MAX_REPO_BUNDLE_BYTES {
        let _ = process.process.signal(pc::Signal::Kill).await;
        return Err(pc::SandboxError::new("repository bundle exceeds limit"));
    }
    let status = process.process.wait().await?;
    if status.code != Some(0) {
        return Err(pc::SandboxError::new(format!(
            "container repository export exited {:?}",
            status.code
        )));
    }
    let url = url.to_string();
    let token = token.map(str::to_string);
    tokio::task::spawn_blocking(move || {
        awaken_sandbox_local::push_repo_bundle(&bundle, &url, token.as_deref())
    })
    .await
    .map_err(|error| pc::SandboxError::new(error.to_string()))?
}
