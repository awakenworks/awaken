//! Exact, ambient-free host Git transport for Repository realization.

mod command;

use std::path::{Path, PathBuf};

#[cfg(test)]
use command::validate_credentialed_git_transport;
use command::{credentialed_git_run, credentialed_git_run_with_alternate, credentialed_git_stdout};
pub(crate) use command::{git_bytes, git_stdout, run_git};

use awaken_provisioning_contract as pc;
use awaken_sandbox_fs::publish_directory_noreplace;

use crate::{IsolatedRoot, SandboxError, jailed_at};

/// Clone a git repository into `<root>/<logical>` **host-side** (ADR-0038). The
/// git process is the only credential holder: one command-scoped credential
/// helper supplies Basic auth while `clone` receives and persists only the clean
/// URL. The complete tree is built and verified in a sibling staging directory,
/// then published with one atomic no-replace rename; the Agent-visible
/// destination therefore never exposes a checkout-in-progress and an object
/// racing into that name is never overwritten.
pub(crate) fn provision_repo_at(
    root: &IsolatedRoot,
    logical: &str,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<(), SandboxError> {
    let dest = jailed_at(root, logical)?;
    let destination_state =
        repository_destination_state(&dest, url, initial_branch, initial_commit)?;
    match destination_state {
        RepositoryDestinationState::Exact => return Ok(()),
        RepositoryDestinationState::CompleteDifferent => {
            return Err(different_repository_error(&dest));
        }
        RepositoryDestinationState::Incomplete => {
            return Err(SandboxError(format!(
                "repository destination `{}` is occupied by an incomplete or non-Repository tree",
                dest.display()
            )));
        }
        RepositoryDestinationState::Absent => {}
    }

    let parent = dest.parent().ok_or_else(|| {
        SandboxError(format!(
            "repository destination `{}` has no parent",
            dest.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| SandboxError(error.to_string()))?;
    let stage_guard = repository_stage(parent)?;
    let stage = stage_guard.path().to_path_buf();
    let mut args = vec!["clone".to_string(), "--no-checkout".to_string()];
    if let Some(branch) = initial_branch {
        args.push("--branch".into());
        args.push(branch.to_string());
    }
    args.push("--".into());
    args.push(url.to_string());
    args.push(stage.to_string_lossy().into_owned());
    let realization = (|| {
        credentialed_git_run(url, &args, credential)?;
        if let Some(commit) = initial_commit {
            run_git(Some(&stage), &["checkout", "--detach", commit])?;
        } else {
            // Populate the selected/default branch only after the credentialed
            // process has exited. Repository filters and checkout behavior never
            // inherit the operation credential.
            run_git(Some(&stage), &["reset", "--hard", "HEAD"])?;
        }
        if !realized_repository_matches(&stage, url, initial_branch, initial_commit) {
            return Err(SandboxError(
                "staged repository does not match the exact realization plan".into(),
            ));
        }
        Ok(())
    })();
    if let Err(error) = realization {
        return Err(cleanup_error(error, stage_guard));
    }

    // Re-evaluate after the potentially long network operation. A concurrent
    // exact publication wins; a complete different tree is never overwritten.
    let destination_state =
        match repository_destination_state(&dest, url, initial_branch, initial_commit) {
            Ok(state) => state,
            Err(error) => return Err(cleanup_error(error, stage_guard)),
        };
    match destination_state {
        RepositoryDestinationState::Exact => {
            return stage_guard.close().map_err(|error| {
                SandboxError(format!(
                    "remove current-attempt Repository stage `{}`: {error}",
                    stage.display()
                ))
            });
        }
        RepositoryDestinationState::CompleteDifferent => {
            let error = different_repository_error(&dest);
            return Err(cleanup_error(error, stage_guard));
        }
        RepositoryDestinationState::Incomplete => {
            let error = SandboxError(format!(
                "repository destination `{}` became occupied before atomic publication",
                dest.display()
            ));
            return Err(cleanup_error(error, stage_guard));
        }
        RepositoryDestinationState::Absent => {}
    }

    if let Err(error) = publish_directory_noreplace(&stage, &dest) {
        let error = SandboxError(format!(
            "atomically publish staged repository `{}` to absent destination `{}`: {error}",
            stage.display(),
            dest.display()
        ));
        if repository_destination_state(&dest, url, initial_branch, initial_commit)
            .is_ok_and(|state| state == RepositoryDestinationState::Exact)
        {
            return stage_guard.close().map_err(|cleanup| {
                SandboxError(format!(
                    "{error}; exact concurrent Repository won but current stage `{}` could not be removed: {cleanup}",
                    stage.display()
                ))
            });
        }
        return Err(cleanup_error(error, stage_guard));
    }
    // The kernel moved exactly the name owned by this TempDir. Disarm its old
    // path cleanup before verifying the new final name; post-verification failure
    // is fail-closed and never grants authority to remove the published tree.
    let _published_stage = stage_guard.keep();
    if realized_repository_matches(&dest, url, initial_branch, initial_commit) {
        Ok(())
    } else {
        Err(SandboxError(format!(
            "atomically published Repository `{}` failed exact post-publication verification",
            dest.display()
        )))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RepositoryDestinationState {
    Absent,
    Exact,
    CompleteDifferent,
    Incomplete,
}

fn repository_destination_state(
    destination: &Path,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
) -> Result<RepositoryDestinationState, SandboxError> {
    let metadata = match std::fs::symlink_metadata(destination) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(RepositoryDestinationState::Absent);
        }
        Err(error) => return Err(SandboxError(error.to_string())),
    };
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Ok(RepositoryDestinationState::Incomplete);
    }
    if realized_repository_matches(destination, url, initial_branch, initial_commit) {
        Ok(RepositoryDestinationState::Exact)
    } else if complete_repository(destination) {
        Ok(RepositoryDestinationState::CompleteDifferent)
    } else {
        Ok(RepositoryDestinationState::Incomplete)
    }
}

fn complete_repository(destination: &Path) -> bool {
    git_stdout(Some(destination), &["rev-parse", "--is-inside-work-tree"])
        .is_ok_and(|value| value.trim() == "true")
        && git_stdout(Some(destination), &["remote", "get-url", "origin"]).is_ok()
        && git_stdout(
            Some(destination),
            &["rev-parse", "--verify", "HEAD^{commit}"],
        )
        .is_ok()
}

fn repository_stage(parent: &Path) -> Result<tempfile::TempDir, SandboxError> {
    tempfile::Builder::new()
        .prefix(".awaken-repository-")
        .suffix(".stage")
        .tempdir_in(parent)
        .map_err(|error| SandboxError(error.to_string()))
}

fn cleanup_error(error: SandboxError, stage: tempfile::TempDir) -> SandboxError {
    let path = stage.path().to_path_buf();
    match stage.close() {
        Ok(()) => error,
        Err(cleanup) => SandboxError(format!(
            "{error}; failed to remove owned repository stage `{}`: {cleanup}",
            path.display()
        )),
    }
}

fn different_repository_error(destination: &Path) -> SandboxError {
    SandboxError(format!(
        "repository destination `{}` already contains a complete different realization",
        destination.display()
    ))
}

fn realized_repository_matches(
    destination: &Path,
    url: &str,
    initial_branch: Option<&str>,
    initial_commit: Option<&str>,
) -> bool {
    if !std::fs::symlink_metadata(destination)
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        return false;
    }
    if !std::fs::symlink_metadata(destination.join(".git"))
        .is_ok_and(|metadata| metadata.is_dir() && !metadata.file_type().is_symlink())
    {
        return false;
    }
    if !git_stdout(Some(destination), &["remote", "get-url", "origin"])
        .is_ok_and(|actual| actual.trim() == url)
    {
        return false;
    }
    if git_stdout(
        Some(destination),
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .is_err()
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
    objects: PathBuf,
}

fn repository_publish_source(
    root: &IsolatedRoot,
    destination: &Path,
    expectation: &awaken_provisioning_contract::RepositoryPublicationExpectation,
) -> Result<RepositoryPublishSource, SandboxError> {
    expectation
        .validate()
        .map_err(|error| SandboxError(error.0))?;
    run_git(None, &["check-ref-format", "--branch", &expectation.branch])?;
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
    if branch != expectation.branch {
        return Err(SandboxError(format!(
            "repository current branch `{branch}` does not match expected branch `{}`",
            expectation.branch
        )));
    }
    let commit = git_stdout(
        Some(destination),
        &["rev-parse", "--verify", "HEAD^{commit}"],
    )?;
    let commit = commit.trim();
    if commit.len() != 40 || !commit.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(SandboxError(
            "repository HEAD did not resolve to a full 40-hex object identifier".into(),
        ));
    }
    if commit != expectation.commit {
        return Err(SandboxError(format!(
            "repository HEAD `{commit}` does not match expected commit `{}`",
            expectation.commit
        )));
    }

    Ok(RepositoryPublishSource { objects })
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

/// Parse Git's total observation for one exact remote ref.
fn observed_remote_commit(
    output: &str,
    expected_ref: &str,
) -> Result<Option<String>, SandboxError> {
    if output.is_empty() {
        return Ok(None);
    }
    let mut lines = output.lines();
    let line = lines.next().expect("non-empty output has one line");
    if lines.next().is_some() {
        return Err(SandboxError(
            "repository remote returned multiple rows for one exact ref".into(),
        ));
    }
    let Some((commit, remote_ref)) = line.split_once('\t') else {
        return Err(SandboxError(
            "repository remote returned a malformed exact-ref observation".into(),
        ));
    };
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        || remote_ref != expected_ref
    {
        return Err(SandboxError(
            "repository remote returned a malformed exact-ref observation".into(),
        ));
    }
    Ok(Some(commit.to_owned()))
}

fn observe_remote_commit(
    remote_url: &str,
    remote_ref: &str,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<Option<String>, SandboxError> {
    let output = credentialed_git_stdout(
        remote_url,
        &["ls-remote", "--", remote_url, remote_ref],
        credential,
    )?;
    observed_remote_commit(&output, remote_ref)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepositoryPublicationAdmission {
    Published,
    Push,
    Rejected(pc::RepositoryPublicationRejection),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RepositoryPublicationReobservation {
    Published,
    Unchanged,
    Rejected(pc::RepositoryPublicationRejection),
}

fn stale_publication_observation(
    expectation: &pc::RepositoryPublicationExpectation,
    observed_commit: Option<&str>,
) -> pc::RepositoryPublicationRejection {
    let rejection = match observed_commit {
        Some(observed_commit) => pc::RepositoryPublicationRejection::RemoteRefChanged {
            observed_commit: observed_commit.to_owned(),
        },
        None => pc::RepositoryPublicationRejection::RemoteRefAbsent,
    };
    debug_assert!(rejection.verify(expectation).is_ok());
    rejection
}

fn admit_repository_publication(
    expectation: &pc::RepositoryPublicationExpectation,
    observed_commit: Option<&str>,
) -> RepositoryPublicationAdmission {
    match observed_commit {
        Some(commit) if commit == expectation.commit => RepositoryPublicationAdmission::Published,
        None if expectation.expected_prior_commit.is_none() => RepositoryPublicationAdmission::Push,
        Some(commit) if expectation.expected_prior_commit.as_deref() == Some(commit) => {
            RepositoryPublicationAdmission::Push
        }
        observed => RepositoryPublicationAdmission::Rejected(stale_publication_observation(
            expectation,
            observed,
        )),
    }
}

fn classify_repository_publication_reobservation(
    expectation: &pc::RepositoryPublicationExpectation,
    before: Option<&str>,
    after: Option<&str>,
) -> RepositoryPublicationReobservation {
    if after == Some(expectation.commit.as_str()) {
        return RepositoryPublicationReobservation::Published;
    }
    if after == before {
        return RepositoryPublicationReobservation::Unchanged;
    }
    RepositoryPublicationReobservation::Rejected(stale_publication_observation(expectation, after))
}

/// Publish the Agent-authored current branch to the exact host-frozen remote.
/// Agent-writable origin, URL rewrite, header, credential, and hook configuration
/// is never consulted by a network Git process. The push runs from a temporary,
/// config-free bare Git directory whose sole source ref names the exact observed
/// commit and whose read-only object alternate is the in-tree clone object store.
/// An explicit absent-ref lease also prevents a concurrent remote creator from
/// turning the checked creation into an update.
pub(crate) fn push_repo_to_at(
    root: &IsolatedRoot,
    logical: &str,
    plan: &awaken_provisioning_contract::RepositoryRealizationPlan,
    expectation: &awaken_provisioning_contract::RepositoryPublicationExpectation,
    credential: Option<&awaken_provisioning_contract::RepositoryHttpBasicCredential>,
) -> Result<pc::RepositoryPublicationReceipt, pc::RepositoryPublicationError> {
    let unavailable = |error: SandboxError| {
        pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(error.0))
    };
    expectation
        .validate()
        .map_err(pc::RepositoryPublicationError::Unavailable)?;
    if plan.access == awaken_provisioning_contract::MountAccess::ReadOnly {
        return Err(pc::RepositoryPublicationError::Unavailable(
            pc::SandboxError::new("read-only repository cannot be published"),
        ));
    }
    let dest = jailed_at(root, logical).map_err(unavailable)?;
    let source = repository_publish_source(root, &dest, expectation).map_err(unavailable)?;
    let remote_ref = format!("refs/heads/{}", expectation.branch);
    let observed =
        observe_remote_commit(&plan.transport_url, &remote_ref, credential).map_err(unavailable)?;
    match admit_repository_publication(expectation, observed.as_deref()) {
        RepositoryPublicationAdmission::Published => {
            return Ok(
                awaken_provisioning_contract::RepositoryPublicationReceipt::new(plan, expectation),
            );
        }
        RepositoryPublicationAdmission::Push => {}
        RepositoryPublicationAdmission::Rejected(rejection) => {
            return Err(pc::RepositoryPublicationError::Rejected(rejection));
        }
    }

    let clean_git = ephemeral_publish_git_dir(&expectation.commit).map_err(unavailable)?;
    let git_dir = clean_git.path().join("git");
    let git_dir_arg = format!("--git-dir={}", git_dir.display());
    let expected_remote = observed.as_deref().unwrap_or_default();
    let exact_ref_lease = format!("--force-with-lease={remote_ref}:{expected_remote}");
    let refspec = format!(
        "refs/heads/awaken-publish:refs/heads/{}",
        expectation.branch
    );
    let push = credentialed_git_run_with_alternate(
        &plan.transport_url,
        &[
            git_dir_arg,
            "push".into(),
            exact_ref_lease,
            "--".into(),
            plan.transport_url.clone(),
            refspec,
        ],
        credential,
        &source.objects,
    );
    let after =
        observe_remote_commit(&plan.transport_url, &remote_ref, credential).map_err(unavailable)?;
    match classify_repository_publication_reobservation(
        expectation,
        observed.as_deref(),
        after.as_deref(),
    ) {
        RepositoryPublicationReobservation::Published => {
            Ok(awaken_provisioning_contract::RepositoryPublicationReceipt::new(plan, expectation))
        }
        RepositoryPublicationReobservation::Unchanged => Err(
            pc::RepositoryPublicationError::Unavailable(pc::SandboxError::new(match push {
                Ok(()) => {
                    "repository remote did not confirm the expected commit after push".to_owned()
                }
                Err(error) => error.0,
            })),
        ),
        RepositoryPublicationReobservation::Rejected(rejection) => {
            Err(pc::RepositoryPublicationError::Rejected(rejection))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{BufRead as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// Local/Namespace Repository crash-cut cause/effect graph and decision
    /// table. Causes: C1 final is absent/exact/complete-different/incomplete;
    /// C2 a current-attempt stage is exact/failed; C3 a name appears after the
    /// preflight but before publish. Effects: E1 publish only absent with the
    /// kernel's no-replace primitive; E2 exact is an idempotent no-op; E3 every
    /// non-exact occupied final is preserved and rejected; E4 an ordinary
    /// failure removes only the current attempt's random stage; E5 a process
    /// crash can leave only that random stage for sandbox terminal cleanup.
    /// Rules: R1 absent+exact-stage=>E1; R2 exact=>E2; R3/R4
    /// complete-different|incomplete=>E3; R5 C3=>E3; R6 failed-stage=>E4.
    /// This test owns R4/R5; provider replay tests own R1-R3, and the checkout
    /// failure test owns R6. R5 is the collision that a check-then-rename misses.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn repository_publication_never_replaces_an_occupied_final_name() {
        let temporary = tempfile::tempdir().expect("temporary sandbox parent");
        let jail = temporary.path().join("jail");
        let workspace = jail.join("workspace");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let root = IsolatedRoot::new(&jail);
        let incomplete = workspace.join("incomplete");
        std::fs::create_dir(&incomplete).expect("occupied incomplete destination");
        std::fs::write(incomplete.join("PRESERVED"), "user bytes").expect("incomplete marker");

        provision_repo_at(
            &root,
            "workspace/incomplete",
            "https://network-must-not-run.invalid/repository",
            None,
            None,
            None,
        )
        .expect_err("R4 incomplete final rejects before Git");
        assert_eq!(
            std::fs::read_to_string(incomplete.join("PRESERVED")).unwrap(),
            "user bytes",
            "R4 final bytes are never removed"
        );

        let stage = workspace.join(".awaken-repository-current-attempt.stage");
        let raced_final = workspace.join("raced-final");
        std::fs::create_dir(&stage).expect("current attempt stage");
        std::fs::write(stage.join("STAGED"), "stage bytes").expect("stage marker");
        std::fs::create_dir(&raced_final).expect("racing final");
        std::fs::write(raced_final.join("PRESERVED"), "racing bytes").expect("racing marker");

        publish_directory_noreplace(&stage, &raced_final)
            .expect_err("R5 kernel rejects an occupied name");
        assert_eq!(
            std::fs::read_to_string(stage.join("STAGED")).unwrap(),
            "stage bytes",
            "R5 current stage remains available to its owner"
        );
        assert_eq!(
            std::fs::read_to_string(raced_final.join("PRESERVED")).unwrap(),
            "racing bytes",
            "R5 racing final is not replaced"
        );
    }

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
            std::fs::read_dir(root.root().join("workspace"))
                .unwrap()
                .filter_map(Result::ok)
                .all(|entry| {
                    let name = entry.file_name();
                    let name = name.to_string_lossy();
                    !name.starts_with(".awaken-repository-") || !name.ends_with(".stage")
                }),
            "R6/C2 ordinary failure removes only its current-attempt stage"
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
        let commit = git_stdout(Some(&repo), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        run_git(Some(&repo), &["checkout", "--detach"]).unwrap();
        let root = IsolatedRoot::new(tmp.path());
        let plan = awaken_provisioning_contract::RepositoryRealizationPlan {
            repository_id: "repo".into(),
            mount_path: "/workspace/repo".into(),
            source_remote_url: "https://invalid.example/repo".into(),
            transport_url: "https://invalid.example/repo".into(),
            initial_branch: None,
            initial_commit: None,
            access: awaken_provisioning_contract::MountAccess::ReadWrite,
        };
        let expectation = awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "master".into(),
            commit,
            expected_prior_commit: None,
        };
        let error = push_repo_to_at(&root, "repo", &plan, &expectation, None)
            .expect_err("detached HEAD rejects before network");
        assert!(matches!(
            error,
            awaken_provisioning_contract::RepositoryPublicationError::Unavailable(error)
                if error.0.contains("detached HEAD")
        ));
    }

    /// CAS observation cause/effect table, including an ambiguous failed push.
    /// The push process result never authorizes success or permanent rejection:
    /// only the exact post-attempt remote observation does. Thus a lost response
    /// with desired `after` is absorbed, an unchanged lease stays retryable, and
    /// a third value is the only durable stale outcome.
    #[test]
    fn repository_publication_reobservation_classifies_response_loss_exactly() {
        let desired = "1111111111111111111111111111111111111111";
        let prior = "2222222222222222222222222222222222222222";
        let third = "3333333333333333333333333333333333333333";
        let create = pc::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: desired.into(),
            expected_prior_commit: None,
        };
        let update = pc::RepositoryPublicationExpectation {
            expected_prior_commit: Some(prior.into()),
            ..create.clone()
        };

        assert_eq!(
            admit_repository_publication(&create, None),
            RepositoryPublicationAdmission::Push,
            "C1 create-only plus absent ref admits only an absent lease"
        );
        assert_eq!(
            admit_repository_publication(&update, Some(prior)),
            RepositoryPublicationAdmission::Push,
            "C2 update admits only the frozen prior commit lease"
        );
        assert_eq!(
            admit_repository_publication(&update, None),
            RepositoryPublicationAdmission::Rejected(
                pc::RepositoryPublicationRejection::RemoteRefAbsent
            ),
            "C3 missing update lease is permanently stale"
        );
        assert!(matches!(
            admit_repository_publication(&update, Some(third)),
            RepositoryPublicationAdmission::Rejected(
                pc::RepositoryPublicationRejection::RemoteRefChanged { observed_commit }
            ) if observed_commit == third
        ));

        for (expectation, before) in [(&create, None), (&update, Some(prior))] {
            assert_eq!(
                classify_repository_publication_reobservation(expectation, before, Some(desired)),
                RepositoryPublicationReobservation::Published,
                "E1 failed-push response loss is absorbed only after exact desired reobservation"
            );
            assert_eq!(
                classify_repository_publication_reobservation(expectation, before, before),
                RepositoryPublicationReobservation::Unchanged,
                "E2 unchanged precondition remains retryable after any push result"
            );
            assert!(matches!(
                classify_repository_publication_reobservation(expectation, before, Some(third)),
                RepositoryPublicationReobservation::Rejected(
                    pc::RepositoryPublicationRejection::RemoteRefChanged { observed_commit }
                ) if observed_commit == third
            ));
        }
        assert_eq!(
            classify_repository_publication_reobservation(&update, Some(prior), None),
            RepositoryPublicationReobservation::Rejected(
                pc::RepositoryPublicationRejection::RemoteRefAbsent
            ),
            "E3 a vanished update lease is permanently stale"
        );
    }

    #[test]
    fn uppercase_publication_oids_fail_before_any_git_effect() {
        // The missing local Repository and unreachable remote are deliberate:
        // both uppercase command variants must fail at the canonical wire
        // admission before filesystem discovery, remote observation, or push.
        let tmp = tempfile::tempdir().unwrap();
        let root = IsolatedRoot::new(tmp.path());
        let plan = pc::RepositoryRealizationPlan {
            repository_id: "repo".into(),
            mount_path: "missing".into(),
            source_remote_url: "https://invalid.example/repo".into(),
            transport_url: "https://invalid.example/repo".into(),
            initial_branch: None,
            initial_commit: None,
            access: pc::MountAccess::ReadWrite,
        };
        let canonical = "0123456789abcdef0123456789abcdef01234567";
        for expectation in [
            pc::RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: "ABCDEF0123456789abcdef0123456789abcdef01".into(),
                expected_prior_commit: None,
            },
            pc::RepositoryPublicationExpectation {
                branch: "awf/work".into(),
                commit: canonical.into(),
                expected_prior_commit: Some("ABCDEF0123456789abcdef0123456789abcdef01".into()),
            },
        ] {
            assert!(matches!(
                push_repo_to_at(&root, "missing", &plan, &expectation, None),
                Err(pc::RepositoryPublicationError::Unavailable(error))
                    if error.0.contains("canonical lowercase 40-hex")
            ));
        }
    }

    /// Explicit Repository publication cause/effect graph and decision table.
    /// Causes: C1 plan is writable; C2 local symbolic branch matches; C3 local
    /// full commit matches; C4 transport is admitted; C5 exact remote observation
    /// is absent/current/expected-prior/stale; C6 credential contains secret material.
    /// Effects: E1 fail before network/effect; E2 create only under an absent-ref
    /// lease; E3 update only under the caller's exact prior-commit lease; E4 replay
    /// returns the identical receipt; E5 stale observations are typed permanent
    /// rejection and never overwritten; E6 no credential value appears in errors.
    /// Rules: P1 !C1=>E1; P2 C1+!C2=>E1; P3 C1+C2+!C3=>E1;
    /// P4 C1+C2+C3+!C4=>E1+E6; P5 None+absent=>E2; P6 current=>E4;
    /// P7 Some(P)+remote=P=>E3; P8 absent/third-value mismatch=>E5.
    #[test]
    fn explicit_publication_is_exact_idempotent_and_non_overwriting() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        run_git(Some(tmp.path()), &["init", "-q", "repo"]).unwrap();
        run_git(Some(&repo), &["checkout", "-q", "-b", "awf/work"]).unwrap();
        std::fs::write(repo.join("README.md"), "expected").unwrap();
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
                "-q",
                "-m",
                "expected",
            ],
        )
        .unwrap();
        let expected_commit = git_stdout(Some(&repo), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let remote = tmp.path().join("remote.git");
        run_git(
            Some(tmp.path()),
            &["init", "-q", "--bare", remote.to_str().unwrap()],
        )
        .unwrap();
        let root = IsolatedRoot::new(tmp.path());
        let plan = awaken_provisioning_contract::RepositoryRealizationPlan {
            repository_id: "repo-1".into(),
            mount_path: "/workspace/repo".into(),
            source_remote_url: remote.to_string_lossy().into_owned(),
            transport_url: remote.to_string_lossy().into_owned(),
            initial_branch: None,
            initial_commit: None,
            access: awaken_provisioning_contract::MountAccess::ReadWrite,
        };
        let expectation = awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: expected_commit.clone(),
            expected_prior_commit: None,
        };

        let readonly = awaken_provisioning_contract::RepositoryRealizationPlan {
            access: awaken_provisioning_contract::MountAccess::ReadOnly,
            transport_url: "ssh://network-must-not-run.invalid/repo".into(),
            ..plan.clone()
        };
        let error = push_repo_to_at(&root, "repo", &readonly, &expectation, None)
            .expect_err("P1 read-only fails before transport");
        assert!(matches!(
            error,
            awaken_provisioning_contract::RepositoryPublicationError::Unavailable(error)
                if error.0.contains("read-only")
        ));

        let wrong_branch = awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/other".into(),
            commit: expected_commit.clone(),
            expected_prior_commit: None,
        };
        let error = push_repo_to_at(&root, "repo", &plan, &wrong_branch, None)
            .expect_err("P2 wrong local branch fails before transport");
        assert!(matches!(
            error,
            awaken_provisioning_contract::RepositoryPublicationError::Unavailable(error)
                if error.0.contains("does not match expected branch")
        ));

        let wrong_commit = awaken_provisioning_contract::RepositoryPublicationExpectation {
            branch: "awf/work".into(),
            commit: "0000000000000000000000000000000000000000".into(),
            expected_prior_commit: None,
        };
        let error = push_repo_to_at(&root, "repo", &plan, &wrong_commit, None)
            .expect_err("P3 wrong local commit fails before transport");
        assert!(matches!(
            error,
            awaken_provisioning_contract::RepositoryPublicationError::Unavailable(error)
                if error.0.contains("does not match expected commit")
        ));

        let rejected_transport = awaken_provisioning_contract::RepositoryRealizationPlan {
            transport_url: "ssh://attacker.invalid/repo".into(),
            ..plan.clone()
        };
        let credential = awaken_provisioning_contract::RepositoryHttpBasicCredential::new(
            "publication-secret-user".to_owned(),
            "publication-secret-password".to_owned(),
        );
        let error = push_repo_to_at(
            &root,
            "repo",
            &rejected_transport,
            &expectation,
            Some(&credential),
        )
        .expect_err("P4 rejected transport cannot publish");
        let rendered = error.to_string();
        assert!(
            rendered.contains("admitted HTTP transport"),
            "P4: {rendered}"
        );
        assert!(!rendered.contains("publication-secret-user"), "P4/E6");
        assert!(!rendered.contains("publication-secret-password"), "P4/E6");

        let first = push_repo_to_at(&root, "repo", &plan, &expectation, None)
            .expect("P5 absent ref is published and confirmed");
        let replay = push_repo_to_at(&root, "repo", &plan, &expectation, None)
            .expect("P6 current ref is an exact replay");
        assert_eq!(first, replay, "P5/P6 identical receipt");
        first.verify(&plan, &expectation).unwrap();

        let divergent = tmp.path().join("divergent");
        run_git(
            Some(tmp.path()),
            &[
                "clone",
                "-q",
                "--branch",
                "awf/work",
                remote.to_str().unwrap(),
                divergent.to_str().unwrap(),
            ],
        )
        .unwrap();
        std::fs::write(divergent.join("README.md"), "divergent").unwrap();
        run_git(
            Some(&divergent),
            &[
                "-c",
                "user.name=Remote Writer",
                "-c",
                "user.email=remote@awaken.local",
                "commit",
                "-q",
                "-am",
                "divergent",
            ],
        )
        .unwrap();
        run_git(Some(&divergent), &["push", "-q", "origin", "awf/work"]).unwrap();
        let divergent_commit = git_stdout(Some(&divergent), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let mut cas_expectation = expectation.clone();
        cas_expectation.expected_prior_commit = Some(divergent_commit.clone());
        let cas = push_repo_to_at(&root, "repo", &plan, &cas_expectation, None)
            .expect("P7 exact prior commit authorizes one CAS update");
        assert_eq!(cas, first, "P7 receipt is independent of create/update");
        let replay = push_repo_to_at(&root, "repo", &plan, &cas_expectation, None)
            .expect("P7 exact desired commit is absorbing replay");
        assert_eq!(cas, replay, "P7 replay receipt");

        std::fs::write(divergent.join("README.md"), "third").unwrap();
        run_git(
            Some(&divergent),
            &[
                "-c",
                "user.name=Remote Writer",
                "-c",
                "user.email=remote@awaken.local",
                "commit",
                "-q",
                "-am",
                "third",
            ],
        )
        .unwrap();
        run_git(
            Some(&divergent),
            &["push", "-q", "--force", "origin", "awf/work"],
        )
        .unwrap();
        let third_commit = git_stdout(Some(&divergent), &["rev-parse", "HEAD"])
            .unwrap()
            .trim()
            .to_owned();
        let error = push_repo_to_at(&root, "repo", &plan, &cas_expectation, None)
            .expect_err("P8 third remote ref is a permanent stale rejection");
        assert!(matches!(
            error,
            awaken_provisioning_contract::RepositoryPublicationError::Rejected(
                awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefChanged {
                    observed_commit
                }
            ) if observed_commit == third_commit
        ));
        let remote_commit = git_stdout(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "rev-parse",
                "refs/heads/awf/work",
            ],
        )
        .unwrap();
        assert_eq!(remote_commit.trim(), third_commit, "P8/E5");

        run_git(
            None,
            &[
                "--git-dir",
                remote.to_str().unwrap(),
                "update-ref",
                "-d",
                "refs/heads/awf/work",
            ],
        )
        .unwrap();
        let error = push_repo_to_at(&root, "repo", &plan, &cas_expectation, None)
            .expect_err("P8 expected prior cannot recreate an absent remote ref");
        assert!(matches!(
            error,
            awaken_provisioning_contract::RepositoryPublicationError::Rejected(
                awaken_provisioning_contract::RepositoryPublicationRejection::RemoteRefAbsent
            )
        ));
    }

    #[test]
    fn exact_remote_observation_rejects_every_noncanonical_shape() {
        // Exact-ref observation causes/effects: empty output means absent; one
        // `<40-hex>\t<expected-ref>` row means present; malformed, multiple, or
        // wrong-ref output fails closed before a push decision.
        let expected_ref = "refs/heads/awf/work";
        let commit = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(observed_remote_commit("", expected_ref).unwrap(), None);
        assert_eq!(
            observed_remote_commit(&format!("{commit}\t{expected_ref}\n"), expected_ref).unwrap(),
            Some(commit.into())
        );
        for malformed in [
            format!("{commit} {expected_ref}\n"),
            format!("short\t{expected_ref}\n"),
            format!("ABCDEF0123456789abcdef0123456789abcdef01\t{expected_ref}\n"),
            format!("{commit}\trefs/heads/other\n"),
            format!("{commit}\t{expected_ref}\n{commit}\t{expected_ref}\n"),
        ] {
            assert!(observed_remote_commit(&malformed, expected_ref).is_err());
        }
    }
}
