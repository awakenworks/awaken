//! Credential-free Git bundle transfer across a container Session boundary.

use awaken_provisioning_contract as pc;
use tokio::io::AsyncReadExt;

const MAX_REPO_BUNDLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_REPOSITORY_BRANCH_BYTES: usize = 1024;
static TRANSFER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) async fn provision(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    logical: &str,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<(), pc::SandboxError> {
    let url_owned = url.to_string();
    let initial_branch_owned = initial_branch.map(str::to_string);
    let initial_commit_owned = initial_commit.map(str::to_string);
    let credential_owned = credential.cloned();
    let bundle = tokio::task::spawn_blocking(move || {
        awaken_sandbox_local::clone_repo_bundle(
            &url_owned,
            initial_branch_owned.as_deref(),
            initial_commit_owned.as_deref(),
            credential_owned.as_ref(),
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
                    "if test -e \"$destination\"; then ",
                    "test \"$(git -C \"$destination\" remote get-url origin)\" = \"$remote\"; ",
                    "if test -n \"$branch\"; then test \"$(git -C \"$destination\" symbolic-ref --short HEAD)\" = \"$branch\"; fi; ",
                    "if test -n \"$commit\"; then test \"$(git -C \"$destination\" rev-parse HEAD)\" = \"$(git -C \"$destination\" rev-parse \"$commit\")\"; fi; ",
                    "exit 0; fi; ",
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
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<pc::RepositoryPublicationReceipt, pc::SandboxError> {
    expectation.validate()?;
    if plan.access == pc::MountAccess::ReadOnly {
        return Err(pc::SandboxError::new(
            "read-only repository cannot be published",
        ));
    }
    let repo = super::container_files::logical_path(&plan.mount_path)?;
    let process = sandbox
        .spawn_agent_process(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                concat!(
                    "set -eu; repo=$1; ",
                    "git -C \"$repo\" symbolic-ref --quiet --short HEAD; ",
                    "git -C \"$repo\" bundle create - --all"
                )
                .into(),
                "awaken-repo-export".into(),
                repo,
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await?;
    let mut bundle = Vec::new();
    process
        .channel
        .take((MAX_REPO_BUNDLE_BYTES + MAX_REPOSITORY_BRANCH_BYTES + 2) as u64)
        .read_to_end(&mut bundle)
        .await
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    if bundle.len() > MAX_REPO_BUNDLE_BYTES + MAX_REPOSITORY_BRANCH_BYTES + 1 {
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
    let branch_end = bundle
        .iter()
        .position(|byte| *byte == b'\n')
        .filter(|index| *index > 0 && *index <= MAX_REPOSITORY_BRANCH_BYTES)
        .ok_or_else(|| pc::SandboxError::new("repository export has no current branch"))?;
    let branch = std::str::from_utf8(&bundle[..branch_end])
        .map_err(|error| pc::SandboxError::new(error.to_string()))?
        .to_owned();
    let bundle = bundle.split_off(branch_end + 1);
    if bundle.len() > MAX_REPO_BUNDLE_BYTES {
        return Err(pc::SandboxError::new("repository bundle exceeds limit"));
    }
    let plan = plan.clone();
    let expectation = expectation.clone();
    let credential = credential.cloned();
    tokio::task::spawn_blocking(move || {
        awaken_sandbox_local::push_repo_bundle(
            &bundle,
            &branch,
            &plan,
            &expectation,
            credential.as_ref(),
        )
    })
    .await
    .map_err(|error| pc::SandboxError::new(error.to_string()))?
}
