//! Startup-only acquisition for ACP protocol wrappers.
//!
//! Discovery owns host capability observation; this module only turns a
//! catalog acquisition declaration into one stable, absolute launch route.

use std::path::Path;

use async_trait::async_trait;
use awaken_run_executor_acp::AcpCli;

#[async_trait]
pub(super) trait AcpWrapperInstaller: Send + Sync {
    async fn resolved_argv(&self, cli: &AcpCli, root: &Path)
    -> Result<Option<Vec<String>>, String>;
}

pub(super) struct NpmWrapperInstaller;

#[async_trait]
impl AcpWrapperInstaller for NpmWrapperInstaller {
    async fn resolved_argv(
        &self,
        cli: &AcpCli,
        root: &Path,
    ) -> Result<Option<Vec<String>>, String> {
        let awaken_run_executor_acp::AcpAcquisition::PinnedNpmWrapper {
            installer,
            package,
            bin,
        } = cli.acquisition
        else {
            return Ok(None);
        };
        let prefix = root.join(cli.id);
        let executable = prefix.join("node_modules").join(".bin").join(bin);
        if executable.is_file() {
            return canonical_wrapper_argv(&executable).map(Some);
        }
        std::fs::create_dir_all(&prefix).map_err(|error| {
            format!("create ACP wrapper directory {}: {error}", prefix.display())
        })?;
        let mut command = tokio::process::Command::new(installer);
        command
            .args([
                "install",
                "--no-audit",
                "--no-fund",
                "--save-exact",
                "--prefix",
            ])
            .arg(&prefix)
            .arg(package)
            .env_clear()
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        for key in ["PATH", "HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        let output = tokio::time::timeout(std::time::Duration::from_secs(120), command.output())
            .await
            .map_err(|_| format!("install pinned ACP wrapper for {} timed out", cli.id))?
            .map_err(|error| format!("start {installer} for {}: {error}", cli.id))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "install pinned ACP wrapper for {} failed: {}",
                cli.id,
                stderr.lines().next().unwrap_or("npm exited unsuccessfully")
            ));
        }
        canonical_wrapper_argv(&executable).map(Some)
    }
}

fn canonical_wrapper_argv(executable: &Path) -> Result<Vec<String>, String> {
    let executable = executable.canonicalize().map_err(|error| {
        format!(
            "installed ACP wrapper {} is unavailable: {error}",
            executable.display()
        )
    })?;
    Ok(vec![executable.to_string_lossy().into_owned()])
}
