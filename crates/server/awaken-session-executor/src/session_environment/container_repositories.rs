//! Credential-free Git bundle transfer across a container Session boundary.

use awaken_provisioning_contract as pc;
use tokio::io::AsyncReadExt;

const MAX_REPO_BUNDLE_BYTES: usize = 256 * 1024 * 1024;
const MAX_REPOSITORY_BRANCH_BYTES: usize = 1024;
static TRANSFER_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(super) async fn provision(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    publisher_bin: &str,
    plan: &pc::RepositoryRealizationPlan,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<(), pc::SandboxError> {
    // Validate before the host obtains a bundle or exposes a credential. The
    // adapter consumes the exact admitted sandbox path; it never interprets a
    // caller path as a workspace-relative alias.
    plan.validate_mount_path()?;
    let sequence = TRANSFER_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let url_owned = plan.transport_url.clone();
    let initial_branch_owned = plan.initial_branch.clone();
    let initial_commit_owned = plan.initial_commit.clone();
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
    let bundle_path = format!("/tmp/awaken-repo-{sequence}.bundle");
    super::container_files::write(sandbox, &bundle_path, &bundle).await?;
    let command =
        repository_import_command(plan, publisher_bin, bundle_path, uuid::Uuid::new_v4())?;
    let process = sandbox.spawn(command).await?;
    let status = process.wait().await?;
    if status.code == Some(0) {
        sandbox.record_owned_path(&plan.mount_path)?;
        Ok(())
    } else {
        Err(pc::SandboxError::new(format!(
            "container repository import exited {:?}",
            status.code
        )))
    }
}

fn repository_import_command(
    plan: &pc::RepositoryRealizationPlan,
    publisher_bin: &str,
    bundle_path: String,
    stage_identity: uuid::Uuid,
) -> Result<pc::Command, pc::SandboxError> {
    let parent = plan
        .mount_path
        .rsplit_once('/')
        .map(|(parent, _)| parent)
        .filter(|parent| !parent.is_empty())
        .ok_or_else(|| pc::SandboxError::new("Repository destination has no sandbox parent"))?;
    let stage = format!(
        "{parent}/.awaken-repository-{}.stage",
        stage_identity.simple()
    );
    Ok(pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            concat!(
                "set -eu; bundle=$1; destination=$2; parent=$3; stage=$4; remote=$5; branch=$6; commit=$7; publisher=$8; ",
                "stage_owned=0; cleanup() { rm -f -- \"$bundle\"; ",
                "if test \"$stage_owned\" = 1; then rm -rf -- \"$stage\"; fi; }; trap cleanup EXIT; ",
                "is_occupied() { test -e \"$1\" || test -L \"$1\"; }; ",
                "is_exact() { candidate=$1; test -d \"$candidate\" && test ! -L \"$candidate\" || return 1; ",
                "test -d \"$candidate/.git\" && test ! -L \"$candidate/.git\" || return 1; ",
                "test \"$(git -C \"$candidate\" remote get-url origin)\" = \"$remote\" || return 1; ",
                "git -C \"$candidate\" rev-parse --verify 'HEAD^{commit}' >/dev/null 2>&1 || return 1; ",
                "if test -n \"$branch\"; then test \"$(git -C \"$candidate\" symbolic-ref --short HEAD)\" = \"$branch\" || return 1; fi; ",
                "if test -n \"$commit\"; then test \"$(git -C \"$candidate\" rev-parse HEAD)\" = \"$(git -C \"$candidate\" rev-parse \"$commit\")\" || return 1; fi; }; ",
                "if is_occupied \"$destination\"; then is_exact \"$destination\" && exit 0; ",
                "echo 'Repository destination is occupied by a non-exact realization' >&2; exit 1; fi; ",
                "mkdir -p -- \"$parent\"; mkdir -m 700 -- \"$stage\"; stage_owned=1; ",
                "git clone --no-checkout -- \"$bundle\" \"$stage\"; ",
                "if test -n \"$branch\"; then git -C \"$stage\" checkout \"$branch\"; fi; ",
                "if test -n \"$commit\"; then git -C \"$stage\" checkout --detach \"$commit\"; else git -C \"$stage\" reset --hard HEAD; fi; ",
                "git -C \"$stage\" remote set-url origin \"$remote\"; is_exact \"$stage\"; ",
                "if is_occupied \"$destination\"; then is_exact \"$destination\" && exit 0; ",
                "echo 'Repository destination became occupied before publication' >&2; exit 1; fi; ",
                "publish_status=0; \"$publisher\" repository-publish-noreplace \"$stage\" \"$destination\" || publish_status=$?; ",
                "if test \"$publish_status\" != 0; then is_exact \"$destination\" && exit 0; exit \"$publish_status\"; fi; ",
                "stage_owned=0; is_exact \"$destination\""
            )
            .into(),
            "awaken-repo-import".into(),
            bundle_path,
            plan.mount_path.clone(),
            parent.into(),
            stage,
            plan.transport_url.clone(),
            plan.initial_branch.clone().unwrap_or_default(),
            plan.initial_commit.clone().unwrap_or_default(),
            publisher_bin.into(),
        ],
        cwd: pc::WorkspaceLayout::ROOT.into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    })
}

pub(super) async fn push(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
    plan.validate_mount_path()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    expectation
        .validate()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    if plan.access == pc::MountAccess::ReadOnly {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new("read-only repository cannot be published"),
        ));
    }
    let repo = plan.mount_path.clone();
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
            cwd: pc::WorkspaceLayout::ROOT.into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    let mut bundle = Vec::new();
    process
        .channel
        .take((MAX_REPO_BUNDLE_BYTES + MAX_REPOSITORY_BRANCH_BYTES + 2) as u64)
        .read_to_end(&mut bundle)
        .await
        .map_err(|error| {
            pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
        })?;
    if bundle.len() > MAX_REPO_BUNDLE_BYTES + MAX_REPOSITORY_BRANCH_BYTES + 1 {
        let _ = process.process.signal(pc::Signal::Kill).await;
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new("repository bundle exceeds limit"),
        ));
    }
    let status = process
        .process
        .wait()
        .await
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    if status.code != Some(0) {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new(format!(
                "container repository export exited {:?}",
                status.code
            )),
        ));
    }
    let branch_end = bundle
        .iter()
        .position(|byte| *byte == b'\n')
        .filter(|index| *index > 0 && *index <= MAX_REPOSITORY_BRANCH_BYTES)
        .ok_or_else(|| {
            pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(
                "repository export has no current branch",
            ))
        })?;
    let branch = std::str::from_utf8(&bundle[..branch_end])
        .map_err(|error| {
            pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
        })?
        .to_owned();
    let bundle = bundle.split_off(branch_end + 1);
    if bundle.len() > MAX_REPO_BUNDLE_BYTES {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new("repository bundle exceeds limit"),
        ));
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
    .map_err(|error| {
        pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
    })?
}

#[cfg(test)]
mod tests {
    use super::*;
}
