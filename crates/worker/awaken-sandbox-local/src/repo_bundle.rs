//! Secret-free repository transfer between a host git transport and a remote sandbox.

use std::io::Write;

use awaken_provisioning_contract as pc;

use crate::{IsolatedRoot, git_bytes, provision_repo_at, push_repo_to_at, run_git};

/// Clone with the host-held credential and serialize all resulting refs into a Git
/// bundle. The persisted origin in the bundle is tokenless.
pub fn clone_repo_bundle(
    url: &str,
    git_ref: Option<&str>,
    token: Option<&str>,
) -> Result<Vec<u8>, pc::SandboxError> {
    let temp = tempfile::tempdir().map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let root = IsolatedRoot::new(temp.path());
    provision_repo_at(&root, "repo", url, git_ref, token)
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    git_bytes(
        Some(&temp.path().join("repo")),
        &["bundle", "create", "-", "--all"],
    )
    .map_err(|error| pc::SandboxError::new(error.to_string()))
}

/// Rehydrate a sandbox-produced Git bundle on the host and push its current branch
/// with the host-held credential. No credential is written into the bundle.
pub fn push_repo_bundle(
    bundle: &[u8],
    remote_url: &str,
    token: Option<&str>,
) -> Result<bool, pc::SandboxError> {
    let temp = tempfile::tempdir().map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let bundle_path = temp.path().join("repo.bundle");
    let mut file = std::fs::File::create(&bundle_path)
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    file.write_all(bundle)
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let repo = temp.path().join("repo");
    let bundle_arg = bundle_path.to_string_lossy().into_owned();
    let repo_arg = repo.to_string_lossy().into_owned();
    run_git(None, &["clone", &bundle_arg, &repo_arg])
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    run_git(Some(&repo), &["remote", "set-url", "origin", remote_url])
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let root = IsolatedRoot::new(temp.path());
    push_repo_to_at(&root, "repo", remote_url, token)
        .map_err(|error| pc::SandboxError::new(error.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    #[test]
    fn bundle_round_trip_pushes_agent_commits_without_a_credential_in_the_bundle() {
        let temp = tempfile::tempdir().unwrap();
        let remote = temp.path().join("remote.git");
        git(temp.path(), &["init", "--bare", remote.to_str().unwrap()]);
        let seed = temp.path().join("seed");
        git(
            temp.path(),
            &["clone", remote.to_str().unwrap(), seed.to_str().unwrap()],
        );
        git(&seed, &["config", "user.name", "test"]);
        git(&seed, &["config", "user.email", "test@example.invalid"]);
        std::fs::write(seed.join("README.md"), "base").unwrap();
        git(&seed, &["add", "README.md"]);
        git(&seed, &["commit", "-m", "base"]);
        git(&seed, &["push", "-u", "origin", "HEAD"]);

        let initial = clone_repo_bundle(remote.to_str().unwrap(), None, None).unwrap();
        let agent = temp.path().join("agent");
        let initial_path = temp.path().join("initial.bundle");
        std::fs::write(&initial_path, initial).unwrap();
        git(
            temp.path(),
            &[
                "clone",
                initial_path.to_str().unwrap(),
                agent.to_str().unwrap(),
            ],
        );
        git(&agent, &["config", "user.name", "agent"]);
        git(&agent, &["config", "user.email", "agent@example.invalid"]);
        std::fs::write(agent.join("README.md"), "changed").unwrap();
        git(&agent, &["add", "README.md"]);
        git(&agent, &["commit", "-m", "agent change"]);
        let changed = git_bytes(Some(&agent), &["bundle", "create", "-", "--all"]).unwrap();

        assert!(push_repo_bundle(&changed, remote.to_str().unwrap(), None).unwrap());
        assert!(!push_repo_bundle(&changed, remote.to_str().unwrap(), None).unwrap());
        let count = std::process::Command::new("git")
            .args([
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-list",
                "--count",
                "--all",
            ])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&count.stdout).trim(), "2");
    }
}
