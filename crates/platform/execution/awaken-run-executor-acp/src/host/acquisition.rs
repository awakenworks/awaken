//! Preparation and read-only resolution of immutable host ACP installations.
use super::{command, npm_launch, process_output};
use crate::AcpCli;
use async_trait::async_trait;
use awaken_acp_contract::AcpCommand;
use std::path::Path;

/// Acquisition port for an ACP protocol wrapper declared by the canonical catalog.
#[async_trait]
pub trait AcpWrapperInstaller: Send + Sync {
    async fn resolved_argv(&self, cli: &AcpCli, root: &Path)
    -> Result<Option<Vec<String>>, String>;

    async fn prepared_command(
        &self,
        cli: &AcpCli,
        root: &Path,
    ) -> Result<Option<AcpCommand>, String> {
        self.resolved_argv(cli, root)
            .await
            .map(|argv| argv.map(AcpCommand::new))
    }
}

/// Production acquisition adapter for direct host executables and pinned npm wrappers.
pub struct NpmWrapperInstaller;

/// Resolve only an already prepared adapter. Operator diagnostics must never
/// install a package or change a deployment's runtime directories.
#[expect(
    clippy::disallowed_methods,
    reason = "trusted host launch resolves only operator-owned executable locations"
)]
pub fn prepared_host_argv(cli: &AcpCli, root: &Path) -> Result<Option<Vec<String>>, String> {
    match cli.acquisition {
        crate::AcpAcquisition::Direct { executable, args } => {
            command::resolve(executable, args, std::env::var_os("PATH").as_deref())
                .map(|command| Some(command.argv))
        }
        crate::AcpAcquisition::PinnedNpmWrapper { package, bin, .. } => {
            for prefix in [versioned_prefix(cli, root)?, root.join(cli.id)] {
                if installed_wrapper_matches(&prefix, package)
                    && let Ok(argv) = npm_launch::wrapper_argv(&prefix, package, bin)
                {
                    return Ok(Some(argv));
                }
            }
            Ok(None)
        }
    }
}

#[expect(
    clippy::disallowed_methods,
    reason = "trusted host launch resolves only operator-owned executable locations"
)]
fn bind_host_runtime(cli: &AcpCli, argv: Vec<String>) -> Result<AcpCommand, String> {
    let mut command = AcpCommand::new(argv);
    if let Some(key) = cli.host_executable_env {
        let host = command::resolve(
            cli.discovery.version.executable,
            &[],
            std::env::var_os("PATH").as_deref(),
        )?;
        let executable = host.runtime_executable(cli.id)?;
        command.env.insert(
            key.to_string(),
            command::adapter_executable_path(&executable)?,
        );
    }
    Ok(command)
}

pub fn prepared_host_command(cli: &AcpCli, root: &Path) -> Result<Option<AcpCommand>, String> {
    prepared_host_argv(cli, root)?
        .map(|argv| bind_host_runtime(cli, argv))
        .transpose()
}

#[async_trait]
impl AcpWrapperInstaller for NpmWrapperInstaller {
    async fn prepared_command(
        &self,
        cli: &AcpCli,
        root: &Path,
    ) -> Result<Option<AcpCommand>, String> {
        let Some(argv) = self.resolved_argv(cli, root).await? else {
            return Ok(None);
        };
        let mut command = bind_host_runtime(cli, argv)?;
        command.runtime = Some(super::inspect_host_runtime(cli, &command).await?);
        Ok(Some(command))
    }

    async fn resolved_argv(
        &self,
        cli: &AcpCli,
        root: &Path,
    ) -> Result<Option<Vec<String>>, String> {
        if let Some(argv) = prepared_host_argv(cli, root)? {
            return Ok(Some(argv));
        }
        let crate::AcpAcquisition::PinnedNpmWrapper {
            installer,
            package,
            bin,
        } = cli.acquisition
        else {
            return Err("direct ACP executable is unavailable".into());
        };
        let parent = root.join(cli.id);
        std::fs::create_dir_all(&parent).map_err(|error| format!("create ACP cache: {error}"))?;
        let lock_path = parent.join(".prepare.lock");
        let lock = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(lock_path)
            .map_err(|error| format!("open ACP preparation lock: {error}"))?;
        tokio::time::timeout(std::time::Duration::from_secs(300), async {
            loop {
                match lock.try_lock() {
                    Ok(()) => return Ok(()),
                    Err(std::fs::TryLockError::WouldBlock) => {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                    Err(std::fs::TryLockError::Error(error)) => return Err(error.to_string()),
                }
            }
        })
        .await
        .map_err(|_| "ACP preparation lock timed out".to_string())??;
        if let Some(argv) = prepared_host_argv(cli, root)? {
            return Ok(Some(argv));
        }
        let staging = tempfile::Builder::new()
            .prefix(".install-")
            .tempdir_in(&parent)
            .map_err(|error| format!("create ACP installation staging directory: {error}"))?;
        // Cache-only preparation avoids unnecessary registry traffic. A cache
        // miss falls back to one bounded online installation into the same
        // unpublished staging directory, never into a live program directory.
        if install(installer, package, staging.path(), true)
            .await
            .is_err()
        {
            install(installer, package, staging.path(), false).await?;
        }
        if !installed_wrapper_matches(staging.path(), package) {
            return Err(format!("installed ACP wrapper does not match {package}"));
        }
        npm_launch::wrapper_argv(staging.path(), package, bin)?;
        let prefix = versioned_prefix(cli, root)?;
        if prefix.exists() {
            // An invalid cache is retained for diagnosis; an active qualified
            // version is never overwritten. Both moves stay under this cache.
            let quarantine = staging.path().with_extension("unqualified");
            std::fs::rename(&prefix, quarantine)
                .map_err(|error| format!("retain unqualified ACP cache: {error}"))?;
        }
        std::fs::rename(staging.path(), &prefix)
            .map_err(|error| format!("publish ACP installation: {error}"))?;
        let _ = staging.keep();
        drop(lock);
        npm_launch::wrapper_argv(&prefix, package, bin).map(Some)
    }
}

fn versioned_prefix(cli: &AcpCli, root: &Path) -> Result<std::path::PathBuf, String> {
    if cli.id.is_empty()
        || !cli
            .id
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"_-".contains(&c))
    {
        return Err("invalid ACP cache identifier".into());
    }
    let crate::AcpAcquisition::PinnedNpmWrapper { package, .. } = cli.acquisition else {
        return Err("direct ACP programs have no package directory".into());
    };
    let (_, version) = package
        .rsplit_once('@')
        .ok_or("ACP package must have an exact version")?;
    if version.is_empty()
        || !version
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b".-_".contains(&c))
    {
        return Err("invalid ACP package version".into());
    }
    Ok(root.join(cli.id).join(format!(
        "{version}-{}-{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )))
}

async fn install(
    installer: &str,
    package: &str,
    prefix: &Path,
    offline: bool,
) -> Result<(), String> {
    let mut command = npm_launch::installer_command(installer)?;
    command
        .args([
            "install",
            "--no-audit",
            "--no-fund",
            "--save-exact",
            "--prefer-offline",
            "--fetch-retries=1",
            "--fetch-timeout=20000",
            "--maxsockets=2",
            "--prefix",
        ])
        .arg(prefix)
        .arg(package);
    if offline {
        command.arg("--offline");
    }
    command
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    npm_launch::inherit_installer_environment(&mut command);
    let output = process_output::output(&mut command, std::time::Duration::from_secs(120))
        .await
        .map_err(|error| format!("ACP installation process: {error}"))?;
    if output.exit_code != Some(0) {
        return Err(format!(
            "ACP package installation failed (exit {:?}): {}",
            output.exit_code,
            super::diagnostic_summary(&String::from_utf8_lossy(&output.stderr))
        ));
    }
    Ok(())
}

pub(super) fn installed_wrapper_matches(prefix: &Path, package: &str) -> bool {
    let Some((name, version)) = package.rsplit_once('@') else {
        return false;
    };
    if name.is_empty() || version.is_empty() {
        return false;
    }
    let Ok(bytes) = std::fs::read(prefix.join("package.json")) else {
        return false;
    };
    let Ok(manifest) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    manifest
        .get("dependencies")
        .and_then(|dependencies| dependencies.get(name))
        .and_then(serde_json::Value::as_str)
        == Some(version)
}

pub(super) fn canonical_launch_argv(
    executable: &Path,
    args: &[&str],
) -> Result<Vec<String>, String> {
    let executable = executable.canonicalize().map_err(|error| {
        format!(
            "local ACP executable {} is unavailable: {error}",
            executable.display()
        )
    })?;
    Ok(std::iter::once(executable.to_string_lossy().into_owned())
        .chain(args.iter().map(|arg| (*arg).to_string()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn read_only_cache_lookup_never_creates_an_installation() {
        let root = tempfile::tempdir().unwrap();
        let cli = crate::acp_cli("codex").unwrap();
        assert!(prepared_host_argv(cli, root.path()).unwrap().is_none());
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
        let crate::AcpAcquisition::PinnedNpmWrapper { package, .. } = cli.acquisition else {
            panic!()
        };
        let (name, version) = package.rsplit_once('@').unwrap();
        let prefix = versioned_prefix(cli, root.path()).unwrap();
        std::fs::create_dir_all(&prefix).unwrap();
        std::fs::write(
            prefix.join("package.json"),
            serde_json::json!({"dependencies":{name:version}}).to_string(),
        )
        .unwrap();
        assert!(
            prepared_host_argv(cli, root.path()).unwrap().is_none(),
            "a root manifest alone cannot qualify a partial installation"
        );
        assert!(
            prefix.is_dir(),
            "diagnostics must preserve an invalid installation"
        );
    }
}
