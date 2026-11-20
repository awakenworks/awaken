//! Operator-owned native and npm CLI resolution without executing shell shims.
use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

pub(super) struct HostCommand {
    pub argv: Vec<String>,
    package: Option<PathBuf>,
}

#[expect(
    clippy::disallowed_methods,
    reason = "trusted host launch resolves only operator-owned executable locations"
)]
pub(super) fn resolve(
    program: &str,
    args: &[&str],
    path: Option<&OsStr>,
) -> Result<HostCommand, String> {
    let path = path.ok_or("host CLI PATH is unavailable")?;
    if Path::new(program).components().count() > 1 {
        return native(Path::new(program), args);
    }
    let package = crate::known_acp_clis()
        .iter()
        .find(|cli| cli.discovery.version.executable == program)
        .and_then(|cli| cli.discovery.npm_package);
    let mut directories: Vec<_> = std::env::split_paths(path)
        .filter(|p| p.is_absolute())
        .collect();
    #[cfg(windows)]
    if let Some(appdata) = std::env::var_os("APPDATA") {
        let directory = PathBuf::from(appdata).join("npm");
        if !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    if let Some(home) = std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")) {
        let directory = PathBuf::from(home).join(".local/bin");
        if directory.is_absolute() && !directories.contains(&directory) {
            directories.push(directory);
        }
    }
    for directory in directories {
        if let Some(executable) =
            awaken_local_process::resolve_operator_executable(program, directory.as_os_str())
        {
            return native(&executable, args);
        }
        if let Some(package) = package {
            let directory = directory.join("node_modules").join(package);
            if !directory.join("package.json").is_file() {
                continue;
            }
            let entry = package_bin(&directory, package, program)?;
            let node = awaken_local_process::resolve_operator_executable("node", path)
                .ok_or("Node.js is required for the installed npm CLI")?;
            return Ok(HostCommand {
                argv: std::iter::once(node.to_string_lossy().into_owned())
                    .chain(std::iter::once(entry.to_string_lossy().into_owned()))
                    .chain(args.iter().map(|arg| (*arg).to_string()))
                    .collect(),
                package: Some(directory),
            });
        }
    }
    Err(format!("host executable `{program}` is unavailable"))
}

fn native(executable: &Path, args: &[&str]) -> Result<HostCommand, String> {
    let executable = executable
        .canonicalize()
        .map_err(|error| format!("host executable unavailable: {error}"))?;
    Ok(HostCommand {
        argv: std::iter::once(executable.to_string_lossy().into_owned())
            .chain(args.iter().map(|arg| (*arg).to_string()))
            .collect(),
        package: None,
    })
}

pub(super) fn package_bin(directory: &Path, name: &str, bin: &str) -> Result<PathBuf, String> {
    let directory = directory
        .canonicalize()
        .map_err(|error| format!("npm CLI package unavailable: {error}"))?;
    let manifest: serde_json::Value = serde_json::from_slice(
        &std::fs::read(directory.join("package.json"))
            .map_err(|error| format!("read npm CLI manifest: {error}"))?,
    )
    .map_err(|error| format!("decode npm CLI manifest: {error}"))?;
    if manifest["name"].as_str() != Some(name) {
        return Err("npm CLI package identity does not match".into());
    }
    let entry = manifest["bin"]
        .as_str()
        .or_else(|| manifest["bin"][bin].as_str())
        .ok_or("npm CLI has no declared executable entry")?;
    if Path::new(entry).is_absolute() {
        return Err("npm CLI entry must be relative".into());
    }
    let entry = directory
        .join(entry)
        .canonicalize()
        .map_err(|error| format!("npm CLI entry unavailable: {error}"))?;
    if !entry.starts_with(&directory) || !entry.is_file() {
        return Err("npm CLI entry escapes its package".into());
    }
    Ok(entry)
}

impl HostCommand {
    pub(super) fn runtime_executable(&self, name: &str) -> Result<PathBuf, String> {
        let Some(package) = &self.package else {
            return Ok(PathBuf::from(&self.argv[0]));
        };
        let arch = match std::env::consts::ARCH {
            "x86_64" => "x64",
            "aarch64" => "arm64",
            other => other,
        };
        let platform = match std::env::consts::OS {
            "windows" => "win32",
            "macos" => "darwin",
            other => other,
        };
        let extension = if cfg!(windows) { ".exe" } else { "" };
        let triple = match (std::env::consts::OS, std::env::consts::ARCH) {
            ("windows", "x86_64") => "x86_64-pc-windows-msvc",
            ("windows", "aarch64") => "aarch64-pc-windows-msvc",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("macos", "aarch64") => "aarch64-apple-darwin",
            ("linux", "x86_64") => "x86_64-unknown-linux-musl",
            ("linux", "aarch64") => "aarch64-unknown-linux-musl",
            _ => return Err("unsupported host CLI platform".into()),
        };
        let platform_package = format!("@openai/{name}-{platform}-{arch}");
        // Match the selected npm launcher's optional-package-first lookup;
        // a leftover legacy vendor tree must not silently select an older CLI.
        let mut roots = vec![package.join("node_modules").join(&platform_package)];
        if let Some(node_modules) = package.parent().and_then(Path::parent) {
            roots.push(node_modules.join(&platform_package));
        }
        roots.push(package.clone());
        for root in roots {
            for subdirectory in ["bin", name] {
                let candidate = root
                    .join("vendor")
                    .join(triple)
                    .join(subdirectory)
                    .join(format!("{name}{extension}"));
                if candidate.is_file() {
                    return candidate.canonicalize().map_err(|error| error.to_string());
                }
            }
        }
        Err("native payload of the selected npm CLI is unavailable".into())
    }
}

/// The pinned Windows adapter passes CODEX_PATH through cmd.exe. Rust's
/// canonical verbatim prefix is not a shell-compatible executable coordinate.
pub(super) fn adapter_executable_path(path: &Path) -> Result<String, String> {
    let value = path
        .to_str()
        .ok_or("host executable path is not valid UTF-8")?;
    #[cfg(windows)]
    {
        let value = if let Some(unc) = value.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{unc}")
        } else {
            value.strip_prefix(r"\\?\").unwrap_or(value).to_string()
        };
        if value.contains(['%', '"', '\r', '\n']) {
            return Err(
                "host executable path is not supported by the pinned Windows adapter".into(),
            );
        }
        Ok(value)
    }
    #[cfg(not(windows))]
    Ok(value.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn npm_entries_require_exact_package_identity_and_containment() {
        let root = tempfile::tempdir().unwrap();
        let package = root.path().join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("main.js"), "").unwrap();
        std::fs::write(root.path().join("foreign.js"), "").unwrap();
        for (name, entry, accepted) in [
            ("@openai/codex", "main.js", true),
            ("foreign", "main.js", false),
            ("@openai/codex", "../foreign.js", false),
        ] {
            std::fs::write(
                package.join("package.json"),
                serde_json::json!({"name":name,"bin":{"codex":entry}}).to_string(),
            )
            .unwrap();
            assert_eq!(
                package_bin(&package, "@openai/codex", "codex").is_ok(),
                accepted
            );
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::*;
    #[test]
    fn windows_adapter_paths_keep_identity_without_verbatim_prefixes() {
        assert_eq!(
            adapter_executable_path(Path::new(r"\\?\C:\agent files\codex.exe")).unwrap(),
            r"C:\agent files\codex.exe"
        );
        assert_eq!(
            adapter_executable_path(Path::new(r"\\?\UNC\host\share\codex.exe")).unwrap(),
            r"\\host\share\codex.exe"
        );
        assert!(adapter_executable_path(Path::new(r"C:\%OTHER%\codex.exe")).is_err());
    }

    #[test]
    fn npm_only_path_resolves_without_running_cmd_or_powershell() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("npm with spaces");
        let package = bin.join("node_modules/@openai/codex");
        std::fs::create_dir_all(package.join("bin")).unwrap();
        std::fs::write(bin.join("codex.cmd"), "must never execute shell").unwrap();
        std::fs::write(bin.join("codex.ps1"), "must never execute powershell").unwrap();
        std::fs::write(
            package.join("package.json"),
            r#"{"name":"@openai/codex","bin":{"codex":"bin/codex.js"}}"#,
        )
        .unwrap();
        std::fs::write(package.join("bin/codex.js"), "").unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), bin.join("node.exe")).unwrap();
        let path = std::env::join_paths([&bin]).unwrap();
        let command = resolve("codex", &["--version"], Some(&path)).unwrap();
        assert_eq!(command.argv.len(), 3);
        assert_eq!(
            Path::new(&command.argv[0]),
            bin.join("node.exe").canonicalize().unwrap()
        );
        assert_eq!(
            Path::new(&command.argv[1]),
            package.join("bin/codex.js").canonicalize().unwrap()
        );
        assert_eq!(command.argv[2], "--version");
        let payload =
            package.join("node_modules/@openai/codex-win32-x64/vendor/x86_64-pc-windows-msvc/bin");
        std::fs::create_dir_all(&payload).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), payload.join("codex.exe")).unwrap();
        let legacy = package.join("vendor/x86_64-pc-windows-msvc/bin");
        std::fs::create_dir_all(&legacy).unwrap();
        std::fs::copy(std::env::current_exe().unwrap(), legacy.join("codex.exe")).unwrap();
        #[cfg(target_arch = "x86_64")]
        assert_eq!(
            command.runtime_executable("codex").unwrap(),
            payload.join("codex.exe").canonicalize().unwrap()
        );
    }
}
