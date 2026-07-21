//! Path-safe file operations inside a Session-owned container workspace.

use awaken_provisioning_contract as pc;

fn read_only_tree_file_path(root: &str, relative: &str) -> Result<String, pc::SandboxError> {
    if relative.is_empty()
        || relative.contains('\\')
        || std::path::Path::new(relative).is_absolute()
        || std::path::Path::new(relative)
            .components()
            .any(|component| !matches!(component, std::path::Component::Normal(_)))
    {
        return Err(pc::SandboxError::new(
            "unsafe container read-only tree path",
        ));
    }
    Ok(format!("{root}/{relative}"))
}

pub(super) fn workspace_path(subdir: &str) -> Result<String, pc::SandboxError> {
    if subdir.starts_with('/')
        || subdir
            .split('/')
            .any(|component| component == "." || component == "..")
    {
        return Err(pc::SandboxError::new("unsafe container workspace subdir"));
    }
    Ok(if subdir.is_empty() {
        "/workspace".into()
    } else {
        format!("/workspace/{subdir}")
    })
}

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
    if logical.is_empty()
        || logical
            .split('/')
            .any(|component| component == "." || component == "..")
    {
        return Err(pc::SandboxError::new("unsafe container logical path"));
    }
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
    if !logical.starts_with('/') || logical.split('/').any(|part| part == "." || part == "..") {
        return Err(pc::SandboxError::new(
            "unsafe container materialization path",
        ));
    }
    let setup = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "umask 077; mkdir -p -- \"$(dirname -- \"$1\")\" && : > \"$1\"".into(),
                "awaken-materialize".into(),
                logical.to_string(),
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
                    logical.to_string(),
                    encoded,
                ],
                cwd: "/workspace".into(),
                env: Vec::new(),
                stdio: pc::Stdio::Null,
            })
            .await?;
        require_success(append.wait().await?, "container materialization append")?;
    }
    Ok(())
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
    files: &[(String, Vec<u8>)],
) -> Result<(), pc::SandboxError> {
    let root = workspace_path(subdir)?;
    let setup = sandbox
        .spawn(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "current=/workspace; old_ifs=$IFS; IFS=/; for part in $2; do current=\"$current/$part\"; test ! -L \"$current\" || exit 65; done; IFS=$old_ifs; rm -rf -- \"$1\" && mkdir -p -- \"$1\"".into(),
                "awaken-read-only-tree".into(),
                root.clone(),
                subdir.to_string(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    let status = setup.wait().await?;
    if status.code != Some(0) {
        return Err(pc::SandboxError::new(format!(
            "container read-only tree setup exited {:?}",
            status.code
        )));
    }
    for (relative, contents) in files {
        write(
            sandbox,
            &read_only_tree_file_path(&root, relative)?,
            contents,
        )
        .await?;
    }
    let restrict = sandbox
        .spawn(pc::Command {
            argv: vec!["chmod".into(), "-R".into(), "a-w".into(), "--".into(), root],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Null,
        })
        .await?;
    let status = restrict.wait().await?;
    if status.code == Some(0) {
        Ok(())
    } else {
        Err(pc::SandboxError::new(format!(
            "container read-only tree chmod exited {:?}",
            status.code
        )))
    }
}

pub(super) async fn remove(
    sandbox: &dyn awaken_sandbox_container::ContainerEnvironment,
    logical: &str,
) -> Result<(), pc::SandboxError> {
    let path = logical_path(logical)?;
    let process = sandbox
        .spawn(pc::Command {
            argv: vec!["rm".into(), "-rf".into(), "--".into(), path],
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
            "container workspace removal exited {:?}",
            status.code
        )))
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
        for unsafe_path in ["/etc", ".", "..", "outputs/../etc", "./outputs"] {
            assert!(
                workspace_path(unsafe_path).is_err(),
                "accepted {unsafe_path}"
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
}
