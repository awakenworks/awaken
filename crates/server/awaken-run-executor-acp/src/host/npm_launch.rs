//! Resolve pinned npm entry points without executing a shell shim on Windows.
use std::path::Path;
#[cfg(any(windows, test))]
use std::path::PathBuf;
use tokio::process::Command;

#[cfg(any(windows, test))]
pub(super) fn package_entry(prefix: &Path, package: &str, bin: &str) -> Result<PathBuf, String> {
    let (name, version) = package
        .rsplit_once('@')
        .ok_or("npm wrapper has no exact version")?;
    let directory = prefix
        .join("node_modules")
        .join(name)
        .canonicalize()
        .map_err(|error| format!("pinned npm wrapper directory unavailable: {error}"))?;
    let document: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("package.json"))
            .map_err(|error| format!("read pinned npm wrapper manifest: {error}"))?,
    )
    .map_err(|error| format!("decode pinned npm wrapper manifest: {error}"))?;
    if document["version"].as_str() != Some(version) {
        return Err("installed npm wrapper version does not match the exact catalog pin".into());
    }
    let entry = document["bin"]
        .as_str()
        .or_else(|| document["bin"][bin].as_str())
        .ok_or("pinned npm wrapper has no declared bin entry")?;
    if Path::new(entry).is_absolute() {
        return Err("npm wrapper bin must be relative to its package".into());
    }
    let entry = directory
        .join(entry)
        .canonicalize()
        .map_err(|error| format!("pinned npm wrapper entry unavailable: {error}"))?;
    if !entry.starts_with(&directory) || !entry.is_file() {
        return Err("npm wrapper entry escapes its exact package directory".into());
    }
    Ok(entry)
}

#[cfg(windows)]
#[expect(
    clippy::disallowed_methods,
    reason = "the ACP launcher resolves the CLI on the host PATH and passes host variables through to the child process"
)]
fn node() -> Result<PathBuf, String> {
    awaken_local_process::resolve_operator_executable(
        "node",
        std::env::var_os("PATH")
            .as_deref()
            .ok_or("Node.js requires the operator PATH")?,
    )
    .ok_or_else(|| "Node.js executable is unavailable on the operator PATH".into())
}

pub(super) fn wrapper_argv(prefix: &Path, package: &str, bin: &str) -> Result<Vec<String>, String> {
    #[cfg(windows)]
    {
        Ok(vec![
            node()?.to_string_lossy().into_owned(),
            package_entry(prefix, package, bin)?
                .to_string_lossy()
                .into_owned(),
        ])
    }
    #[cfg(not(windows))]
    {
        let _ = package;
        super::canonical_launch_argv(&prefix.join("node_modules/.bin").join(bin), &[])
    }
}

pub(super) fn installer_command(installer: &str) -> Result<Command, String> {
    #[cfg(windows)]
    if installer == "npm" {
        let node = node()?;
        let script = node
            .parent()
            .ok_or("Node.js executable has no parent")?
            .join("node_modules/npm/bin/npm-cli.js");
        let script = script
            .canonicalize()
            .map_err(|error| format!("Node.js npm entry unavailable: {error}"))?;
        let mut command = Command::new(node);
        command.arg(script);
        return Ok(command);
    }
    Ok(Command::new(installer))
}

#[expect(
    clippy::disallowed_methods,
    reason = "the ACP launcher resolves the CLI on the host PATH and passes host variables through to the child process"
)]
pub(super) fn inherit_host_environment(command: &mut Command) {
    for key in ["PATH", "HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    #[cfg(windows)]
    {
        if std::env::var_os("HOME").is_none()
            && let Some(profile) = std::env::var_os("USERPROFILE")
        {
            command.env("HOME", profile);
        }
        for key in [
            "SYSTEMROOT",
            "USERPROFILE",
            "TEMP",
            "TMP",
            "APPDATA",
            "LOCALAPPDATA",
        ] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_npm_manifest_selects_one_contained_entry() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("node_modules/@fixture/adapter");
        std::fs::create_dir_all(package.join("dist")).unwrap();
        std::fs::write(package.join("dist/main.js"), "process.exit(0)").unwrap();
        let manifest = package.join("package.json");
        for bin in [
            serde_json::json!("dist/main.js"),
            serde_json::json!({"adapter":"dist/main.js"}),
        ] {
            std::fs::write(
                &manifest,
                serde_json::json!({"version":"1.2.3","bin":bin}).to_string(),
            )
            .unwrap();
            assert_eq!(
                package_entry(root.path(), "@fixture/adapter@1.2.3", "adapter").unwrap(),
                package.join("dist/main.js").canonicalize().unwrap()
            );
            assert!(package_entry(root.path(), "@fixture/adapter@9.9.9", "adapter").is_err());
        }
        std::fs::write(
            package.parent().unwrap().join("foreign.js"),
            "outside package",
        )
        .unwrap();
        for bin in ["../foreign.js", "absent.js"] {
            std::fs::write(
                &manifest,
                serde_json::json!({"version":"1.2.3","bin":bin}).to_string(),
            )
            .unwrap();
            assert!(package_entry(root.path(), "@fixture/adapter@1.2.3", "adapter").is_err());
        }
    }
}
