//! Path-safe file operations inside a Session-owned container workspace.

use awaken_provisioning_contract as pc;

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
    let mut writer = sandbox
        .spawn_agent_process(pc::Command {
            argv: vec![
                "sh".into(),
                "-c".into(),
                "umask 077; mkdir -p -- \"$(dirname -- \"$1\")\" && cat > \"$1\"".into(),
                "awaken-materialize".into(),
                logical.to_string(),
            ],
            cwd: "/workspace".into(),
            env: Vec::new(),
            stdio: pc::Stdio::Piped,
        })
        .await?;
    use tokio::io::AsyncWriteExt;
    writer
        .channel
        .write_all(contents)
        .await
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    writer
        .channel
        .shutdown()
        .await
        .map_err(|error| pc::SandboxError::new(error.to_string()))?;
    let status = writer.process.wait().await?;
    if status.code == Some(0) {
        Ok(())
    } else {
        Err(pc::SandboxError::new(format!(
            "container materialization exited {:?}",
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
    fn logical_paths_preserve_workspace_without_double_prefixing() {
        assert_eq!(logical_path("repo").unwrap(), "/workspace/repo");
        assert_eq!(logical_path("workspace/repo").unwrap(), "/workspace/repo");
        assert_eq!(logical_path("/workspace/repo").unwrap(), "/workspace/repo");
        for unsafe_path in ["", "/", ".", "..", "repo/../escape", "repo/./file"] {
            assert!(logical_path(unsafe_path).is_err(), "accepted {unsafe_path}");
        }
    }
}
