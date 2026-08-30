//! Secret-free repository transfer between a host git transport and a remote sandbox.

use std::io::Write;

use awaken_provisioning_contract as pc;

use crate::{IsolatedRoot, git_bytes, provision_repo_at, push_repo_to_at, run_git};

/// Clone with the host-held credential and serialize all resulting refs into a Git
/// bundle. The persisted origin in the bundle is tokenless.
pub fn clone_repo_bundle(
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<Vec<u8>, pc::SandboxError> {
    let temp = tempfile::tempdir().map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let root = IsolatedRoot::new(temp.path());
    provision_repo_at(
        &root,
        "repo",
        url,
        initial_branch,
        initial_commit,
        credential,
    )
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
    branch: &str,
    plan: &pc::RepositoryRealizationPlan,
    expectation: &pc::RepositoryPublicationExpectation,
    credential: Option<&pc::RepositoryHttpBasicCredential>,
) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
    expectation
        .validate()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    if branch != expectation.branch {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new(format!(
                "repository export branch `{branch}` does not match expected branch `{}`",
                expectation.branch
            )),
        ));
    }
    let temp = tempfile::tempdir().map_err(|error| {
        pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
    })?;
    let bundle_path = temp.path().join("repo.bundle");
    let mut file = std::fs::File::create(&bundle_path).map_err(|error| {
        pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
    })?;
    file.write_all(bundle).map_err(|error| {
        pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
    })?;
    let repo = temp.path().join("repo");
    let bundle_arg = bundle_path.to_string_lossy().into_owned();
    let repo_arg = repo.to_string_lossy().into_owned();
    run_git(None, &["clone", "--branch", branch, &bundle_arg, &repo_arg]).map_err(|error| {
        pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.to_string()))
    })?;
    let root = IsolatedRoot::new(temp.path());
    push_repo_to_at(&root, "repo", plan, expectation, credential)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git_transport::git_stdout;

    fn git(cwd: &std::path::Path, args: &[&str]) {
        let status = std::process::Command::new("git")
            .current_dir(cwd)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// Repository bundle publication decision table. Causes: C1 exported branch
    /// matches the frozen expectation; C2 expected commit exists in that branch;
    /// C3 the exact remote ref is absent/current/different. Effects: E1 reject a
    /// wrong branch or commit before network; E2 push only an absent exact ref;
    /// E3 return the same secret-free receipt for the first push and replay; E4
    /// reject a different remote ref without overwriting it. Rules: R1 !C1=>E1;
    /// R2 C1+!C2=>E1; R3 C1+C2+absent=>E2+E3; R4 current=>E3; R5 different=>E4.
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
        let base_branch = git_stdout(Some(&seed), &["rev-parse", "--abbrev-ref", "HEAD"]).unwrap();

        let initial = clone_repo_bundle(remote.to_str().unwrap(), None, None, None).unwrap();
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
        let base = git_stdout(Some(&agent), &["rev-parse", "HEAD"]).unwrap();
        git(&agent, &["checkout", "-b", "awf/work"]);
        git(&agent, &["config", "user.name", "agent"]);
        git(&agent, &["config", "user.email", "agent@example.invalid"]);
        std::fs::write(agent.join("README.md"), "changed").unwrap();
        git(&agent, &["add", "README.md"]);
        git(&agent, &["commit", "-m", "agent change"]);
        let work = git_stdout(Some(&agent), &["rev-parse", "HEAD"]).unwrap();
        let changed = git_bytes(Some(&agent), &["bundle", "create", "-", "--all"]).unwrap();
        let plan = pc::RepositoryRealizationPlan {
            repository_id: "repo-1".into(),
            mount_path: "repo".into(),
            source_remote_url: remote.to_string_lossy().into_owned(),
            transport_url: remote.to_string_lossy().into_owned(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        };
        let expectation = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: work.trim().into(),
            expected_prior_commit: None,
        };

        assert!(push_repo_bundle(&changed, "absent", &plan, &expectation, None).is_err());
        let first = push_repo_bundle(&changed, "awf/work", &plan, &expectation, None).unwrap();
        let replay = push_repo_bundle(&changed, "awf/work", &plan, &expectation, None).unwrap();
        assert_eq!(first, replay, "R3/R4 deterministic receipt");
        first.verify(&plan, &expectation).unwrap();

        run_git(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "update-ref",
                "refs/heads/awf/work",
                base.trim(),
            ],
        )
        .unwrap();
        assert!(
            push_repo_bundle(&changed, "awf/work", &plan, &expectation, None).is_err(),
            "R5 even a would-be fast-forward must not overwrite a nonempty ref"
        );
        let observed = git_stdout(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/awf/work",
            ],
        )
        .unwrap();
        assert_eq!(observed.trim(), base.trim(), "R5 ancestor ref unchanged");

        let divergent = temp.path().join("divergent");
        git(
            temp.path(),
            &[
                "clone",
                "--branch",
                "awf/work",
                remote.to_str().unwrap(),
                divergent.to_str().unwrap(),
            ],
        );
        git(&divergent, &["config", "user.name", "remote-writer"]);
        git(
            &divergent,
            &["config", "user.email", "remote@example.invalid"],
        );
        std::fs::write(divergent.join("README.md"), "remote changed").unwrap();
        git(&divergent, &["commit", "-am", "remote change"]);
        git(&divergent, &["push", "origin", "awf/work"]);
        let divergent_commit = git_stdout(Some(&divergent), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        assert!(
            push_repo_bundle(&changed, "awf/work", &plan, &expectation, None).is_err(),
            "R5 a different nonempty remote ref is not overwritten"
        );
        let observed = git_stdout(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/awf/work",
            ],
        )
        .unwrap();
        assert_eq!(observed.trim(), divergent_commit, "R5 remote unchanged");
        let refs = std::process::Command::new("git")
            .args(["--git-dir", remote.to_str().unwrap(), "show-ref", "--heads"])
            .output()
            .unwrap();
        let refs = String::from_utf8(refs.stdout).unwrap();
        assert!(refs.contains(&format!(
            "{} refs/heads/{}",
            base.trim(),
            base_branch.trim()
        )));
        assert!(refs.contains(&format!("{} refs/heads/awf/work", divergent_commit.trim())));
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
