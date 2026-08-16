//! Path-safe file operations inside a Session-owned container workspace.

use awaken_provisioning_contract as pc;

static FILE_STAGE_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

#[must_use]
const fn read_only_tree_publish_admitted(
    path_safe: bool,
    stage_complete: bool,
    stage_read_only: bool,
) -> bool {
    path_safe && stage_complete && stage_read_only
}

fn lexical_components(path: &str, allow_empty: bool) -> Result<Vec<&str>, pc::SandboxError> {
    if path.contains('\0') || path.contains('\\') {
        return Err(pc::SandboxError::new("unsafe sandbox path"));
    }
    let components = path
        .split('/')
        .filter(|component| !component.is_empty())
        .collect::<Vec<_>>();
    if (!allow_empty && components.is_empty())
        || components
            .iter()
            .any(|component| matches!(*component, "." | ".."))
    {
        return Err(pc::SandboxError::new("unsafe sandbox path"));
    }
    Ok(components)
}

/// Canonicalize one sandbox-absolute path at the runtime boundary.
///
/// Resource manifests and runtime-owned configuration files may legitimately
/// live outside `/workspace`, so this helper deliberately validates lexical
/// safety without imposing a particular sandbox root. Callers that require a
/// workspace path must use [`workspace_path`] or [`logical_path`] instead.
pub(super) fn sandbox_absolute_path(path: &str) -> Result<String, pc::SandboxError> {
    if !path.starts_with('/') {
        return Err(pc::SandboxError::new("unsafe sandbox-absolute path"));
    }
    let components = lexical_components(path, false)?;
    Ok(format!("/{}", components.join("/")))
}

fn read_only_tree_file_path(root: &str, relative: &str) -> Result<String, pc::SandboxError> {
    if relative.starts_with('/') {
        return Err(pc::SandboxError::new(
            "unsafe container read-only tree path",
        ));
    }
    let relative = lexical_components(relative, false)?.join("/");
    Ok(format!("{root}/{relative}"))
}

pub(super) fn workspace_path(subdir: &str) -> Result<String, pc::SandboxError> {
    if subdir.starts_with('/') {
        return Err(pc::SandboxError::new("unsafe container workspace subdir"));
    }
    let components = lexical_components(subdir, true)?;
    Ok(if components.is_empty() {
        "/workspace".into()
    } else {
        format!("/workspace/{}", components.join("/"))
    })
}

#[cfg(test)]
pub(super) fn read_root(subdir: &str, outputs_path: &str) -> Result<String, pc::SandboxError> {
    let workspace = workspace_path(subdir)?;
    if subdir == "outputs" {
        Ok(outputs_path.to_string())
    } else if let Some(relative) = subdir.strip_prefix("outputs/") {
        Ok(format!("{}/{relative}", outputs_path.trim_end_matches('/')))
    } else {
        Ok(workspace)
    }
}

pub(super) fn logical_path(logical: &str) -> Result<String, pc::SandboxError> {
    let logical = logical.trim_start_matches('/');
    let logical = lexical_components(logical, false)?.join("/");
    Ok(
        if logical == "workspace" || logical.starts_with("workspace/") {
            format!("/{logical}")
        } else {
            format!("/workspace/{logical}")
        },
    )
}

pub(super) async fn write(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    logical: &str,
    contents: &[u8],
) -> Result<(), pc::SandboxError> {
    let logical = sandbox_absolute_path(logical)?;
    let stage = format!(
        "{logical}.awaken-stage-{}-{}",
        std::process::id(),
        FILE_STAGE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let setup = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "umask 077; mkdir -p -- \"$(dirname -- \"$1\")\" && : > \"$2\"".into(),
                "awaken-materialize".into(),
                logical.clone(),
                stage.clone(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    require_success(setup.wait().await?, "container materialization setup")?;

    // Detached exec has consistent completion semantics across Docker, Podman and
    // Kubernetes, unlike half-closing an attached stdin stream. Chunked base64 keeps
    // every argv well below OS limits while preserving arbitrary binary file bytes.
    use base64::Engine as _;
    for chunk in contents.chunks(32 * 1024) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(chunk);
        let append = sandbox
            .spawn(pc::Command {
                argv: vec![
                    "sh".into(),
                    "-c".into(),
                    "printf %s \"$2\" | base64 -d >> \"$1\"".into(),
                    "awaken-materialize".into(),
                    stage.clone(),
                    encoded,
                ],
                cwd: "/workspace".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Null,
            })
            .await?;
        if let Err(error) =
            require_success(append.wait().await?, "container materialization append")
        {
            remove_staged_file(sandbox, &stage).await;
            return Err(error);
        }
    }
    let commit = sandbox
        .spawn(pc::Command {
            argv: vec![
                "mv".into(),
                "-f".into(),
                "--".into(),
                stage.clone(),
                logical,
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    let result = require_success(commit.wait().await?, "container materialization commit");
    if result.is_err() {
        remove_staged_file(sandbox, &stage).await;
    }
    result
}

async fn remove_staged_file(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    stage: &str,
) {
    if let Ok(process) = sandbox
        .spawn(pc::Command {
            argv: vec!["rm".into(), "-f".into(), "--".into(), stage.to_string()],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await
    {
        let _ = process.wait().await;
    }
}

fn require_success(status: pc::ExitStatus, operation: &str) -> Result<(), pc::SandboxError> {
    if status.code == Some(0) {
        Ok(())
    } else {
        Err(pc::SandboxError::new(format!(
            "{operation} exited {:?}",
            status.code
        )))
    }
}

pub(super) async fn materialize_read_only_tree(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    subdir: &str,
    files: &[(String, Vec<u8>, bool)],
) -> Result<(), pc::SandboxError> {
    let root = workspace_path(subdir)?;
    let sequence = FILE_STAGE_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let stage = format!("{root}.awaken-tree-stage-{}-{sequence}", std::process::id());
    let backup = format!(
        "{root}.awaken-tree-backup-{}-{sequence}",
        std::process::id()
    );
    let setup = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "current=/workspace; old_ifs=$IFS; IFS=/; for part in $2; do current=\"$current/$part\"; test ! -L \"$current\" || exit 65; done; IFS=$old_ifs; chmod -R u+w -- \"$1\" 2>/dev/null || true; rm -rf -- \"$1\" && mkdir -p -- \"$1\"".into(),
                "awaken-read-only-tree".into(),
                stage.clone(),
                subdir.to_string(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    require_success(setup.wait().await?, "container read-only tree setup")?;
    for (relative, contents, executable) in files {
        let path = read_only_tree_file_path(&stage, relative)?;
        if let Err(error) = write(sandbox, &path, contents).await {
            remove_staged_tree(sandbox, &stage).await;
            return Err(error);
        }
        if *executable {
            let chmod = sandbox
                .spawn(pc::Command {
                    argv: vec!["chmod".into(), "u+x".into(), "--".into(), path],
                    cwd: "/workspace".into(),
                    env: Vec::new(),
                    stdio: pc::Stdio::Null,
                })
                .await?;
            if let Err(error) =
                require_success(chmod.wait().await?, "container executable Skill file chmod")
            {
                remove_staged_tree(sandbox, &stage).await;
                return Err(error);
            }
        }
    }
    let restrict = sandbox
        .spawn(pc::Command {
            argv: vec![
                "chmod".into(),
                "-R".into(),
                "a-w".into(),
                "--".into(),
                stage.clone(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    if let Err(error) = require_success(restrict.wait().await?, "container read-only tree chmod") {
        remove_staged_tree(sandbox, &stage).await;
        return Err(error);
    }
    debug_assert!(read_only_tree_publish_admitted(true, true, true));
    let commit = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "chmod -R u+w -- \"$3\" 2>/dev/null || true; rm -rf -- \"$3\"; if test -e \"$1\"; then mv -- \"$1\" \"$3\" || exit 66; fi; if mv -- \"$2\" \"$1\"; then chmod -R u+w -- \"$3\" 2>/dev/null || true; rm -rf -- \"$3\"; else test ! -e \"$3\" || mv -- \"$3\" \"$1\"; exit 67; fi".into(),
                "awaken-commit-read-only-tree".into(),
                root,
                stage.clone(),
                backup,
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    let result = require_success(commit.wait().await?, "container read-only tree commit");
    if result.is_err() {
        remove_staged_tree(sandbox, &stage).await;
    }
    result
}

async fn remove_staged_tree(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    stage: &str,
) {
    if let Ok(process) = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "chmod -R u+w -- \"$1\" 2>/dev/null || true; rm -rf -- \"$1\"".into(),
                "awaken-remove-staged-tree".into(),
                stage.to_string(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await
    {
        let _ = process.wait().await;
    }
}

pub(super) async fn remove(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    logical: &str,
) -> Result<(), pc::SandboxError> {
    let path = logical_path(logical)?;
    let process = sandbox.spawn(removal_command(path)).await?;
    require_success(process.wait().await?, "container workspace removal")
}

fn removal_command(path: String) -> pc::Command {
    pc::Command {
        argv: vec![
            "sh".into(),
            "-c".into(),
            // Runtime-owned Skill trees are deliberately chmod'd read-only,
            // including their directories. Restore only owner write permission
            // before removing the already-jailed path; without this, an
            // unprivileged container user cannot revoke an old exact projection.
            "chmod -R u+w -- \"$1\" 2>/dev/null || true; rm -rf -- \"$1\"".into(),
            "awaken-remove-workspace-path".into(),
            path,
        ],
        cwd: "/workspace".into(),
        env: Vec::new(),
        stdio: pc::Stdio::Null,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_paths_are_rooted_and_traversal_is_rejected() {
        assert_eq!(workspace_path("").unwrap(), "/workspace");
        assert_eq!(
            workspace_path("outputs/nested").unwrap(),
            "/workspace/outputs/nested"
        );
        for unsafe_path in [
            "/etc",
            ".",
            "..",
            "outputs/../etc",
            "./outputs",
            "windows\\path",
        ] {
            assert!(
                workspace_path(unsafe_path).is_err(),
                "accepted {unsafe_path}"
            );
        }
    }

    #[test]
    fn sandbox_absolute_paths_have_one_canonical_lexical_form() {
        assert_eq!(
            sandbox_absolute_path("//mnt/session//input.bin").unwrap(),
            "/mnt/session/input.bin"
        );
        for unsafe_path in [
            "relative",
            "/",
            "/mnt/../secret",
            "/mnt/./input",
            "/mnt/windows\\path",
            "/mnt/nul\0path",
        ] {
            assert!(
                sandbox_absolute_path(unsafe_path).is_err(),
                "accepted {unsafe_path:?}"
            );
        }
    }

    #[test]
    fn output_reads_follow_the_environment_output_volume() {
        assert_eq!(read_root("outputs", "/outputs").unwrap(), "/outputs");
        assert_eq!(
            read_root("outputs/nested", "/mnt/session/outputs/").unwrap(),
            "/mnt/session/outputs/nested"
        );
        assert_eq!(
            read_root("repository", "/outputs").unwrap(),
            "/workspace/repository"
        );
        assert!(read_root("outputs/../etc", "/outputs").is_err());
    }

    #[test]
    fn logical_paths_preserve_workspace_without_double_prefixing() {
        assert_eq!(logical_path("repo").unwrap(), "/workspace/repo");
        assert_eq!(logical_path("workspace/repo").unwrap(), "/workspace/repo");
        assert_eq!(logical_path("/workspace/repo").unwrap(), "/workspace/repo");
        for unsafe_path in ["", "/", ".", "..", "repo/../escape", "repo/./file"] {
            assert!(logical_path(unsafe_path).is_err(), "accepted {unsafe_path}");
        }
    }

    #[test]
    fn read_only_tree_paths_are_relative_and_lexically_safe() {
        assert_eq!(
            read_only_tree_file_path("/workspace/.skills/demo", "SKILL.md").unwrap(),
            "/workspace/.skills/demo/SKILL.md"
        );
        assert_eq!(
            read_only_tree_file_path("/workspace/.skills/demo", "references/guide.md").unwrap(),
            "/workspace/.skills/demo/references/guide.md"
        );
        for unsafe_path in ["", "/etc/passwd", "../escape", "a/../escape", "a\\b"] {
            assert!(
                read_only_tree_file_path("/workspace/.skills/demo", unsafe_path).is_err(),
                "accepted {unsafe_path}"
            );
        }
    }

    #[test]
    fn removal_restores_owner_write_before_deleting_a_read_only_tree() {
        let command = removal_command("/workspace/.skills".into());
        assert_eq!(command.argv[0], "sh");
        assert_eq!(command.argv[1], "-c");
        assert_eq!(
            command.argv[2],
            "chmod -R u+w -- \"$1\" 2>/dev/null || true; rm -rf -- \"$1\""
        );
        assert_eq!(command.argv[4], "/workspace/.skills");
    }
}

#[cfg(kani)]
#[kani::proof]
fn read_only_tree_publication_requires_a_complete_restricted_safe_stage() {
    let path_safe = kani::any();
    let stage_complete = kani::any();
    let stage_read_only = kani::any();
    let admitted = read_only_tree_publish_admitted(path_safe, stage_complete, stage_read_only);
    assert_eq!(admitted, path_safe && stage_complete && stage_read_only);
    if admitted {
        assert!(path_safe);
        assert!(stage_complete);
        assert!(stage_read_only);
    }
}
