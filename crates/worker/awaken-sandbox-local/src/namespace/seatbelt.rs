//! Pure macOS Seatbelt runtime and path projection.

#[cfg_attr(not(test), expect(clippy::wildcard_imports))]
use super::*;

fn runtime_projection_root(entry: PathBuf) -> Option<PathBuf> {
    if !entry.is_absolute()
        || entry.starts_with("/usr")
        || entry.starts_with("/bin")
        || entry.starts_with("/sbin")
    {
        return None;
    }
    let is_bin = entry.file_name().is_some_and(|name| name == "bin");
    let path = entry.to_string_lossy();
    Some(
        if let Some(modules) = entry
            .ancestors()
            .filter(|ancestor| {
                ancestor
                    .file_name()
                    .is_some_and(|name| name == "node_modules")
            })
            .last()
        {
            // npm entrypoints import hoisted siblings and nested dependencies.
            // The outer installation tree is the runtime closure; its parent
            // may contain application data and is deliberately not projected.
            modules.to_path_buf()
        } else if let Some((prefix, _)) = path.split_once("/.local/share/uv/python/") {
            PathBuf::from(format!("{prefix}/.local/share/uv/python"))
        } else if is_bin && (path.contains("/venv/") || path.ends_with("/venv/bin")) {
            entry
                .parent()
                .and_then(std::path::Path::parent)
                .unwrap_or(&entry)
                .to_path_buf()
        } else if is_bin && path.contains("/.nvm/versions/node/") {
            entry.parent().unwrap_or(&entry).to_path_buf()
        } else {
            entry
        },
    )
}

/// Kernel and runtime state trees. A projected runtime never lives here, and
/// their contents (process views, device nodes, service sockets) are exactly
/// what the Namespace boundary withholds.
const HOST_STATE_ROOTS: [&str; 5] = ["/proc", "/sys", "/dev", "/run", "/var/run"];

/// A PATH-derived runtime root is exposed read-only in full, so it must name an
/// installation tree, never a region of host state. Rejected: a relative or
/// non-normal path (`..`, `.`), `/` and any other single-component root (which
/// would expose a whole top-level tree such as `/home` or `/var`), the operator
/// home or `/root` or any ancestor of either (which would expose credentials and
/// every sibling project), and anything inside kernel/runtime state trees.
/// Installation trees *below* a home (`~/.nvm/versions/node/vX`,
/// `~/.local/bin`) remain projectable.
pub(super) fn is_projectable_runtime_root(
    root: &std::path::Path,
    home: Option<&std::path::Path>,
) -> bool {
    use std::path::Component;
    if !root.is_absolute()
        || root
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return false;
    }
    let depth = root
        .components()
        .filter(|component| matches!(component, Component::Normal(_)))
        .count();
    if depth < 2 {
        return false;
    }
    let exposes = |protected: &std::path::Path| protected.starts_with(root);
    if home.is_some_and(exposes) || exposes(std::path::Path::new("/root")) {
        return false;
    }
    if let Some(relative) = home.and_then(|home| root.strip_prefix(home).ok()) {
        // A whole first-level home tree is not a runtime closure (for example,
        // .cargo also carries credentials beside bin). Known installation
        // subtrees stay possible; private state trees never become runtimes.
        if relative.components().count() < 2
            || [
                ".ssh", ".aws", ".azure", ".config", ".gnupg", ".docker", ".kube", ".codex",
                ".claude",
            ]
            .iter()
            .any(|private| relative.starts_with(private))
        {
            return false;
        }
    }
    !HOST_STATE_ROOTS.iter().any(|state| root.starts_with(state))
}

#[expect(
    clippy::disallowed_methods,
    reason = "the seatbelt profile admits the runtime roots under the host HOME"
)]
fn push_runtime_projection(roots: &mut Vec<String>, entry: PathBuf) {
    let Some(root) = runtime_projection_root(entry) else {
        return;
    };
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .filter(|home| home.is_absolute());
    if !is_projectable_runtime_root(&root, home.as_deref()) {
        return;
    }
    let root = root.to_string_lossy().into_owned();
    if !roots.contains(&root) {
        roots.push(root);
    }
}

pub(super) fn projected_runtime_roots(env: &[(String, String)]) -> Vec<String> {
    let Some(path) = env
        .iter()
        .rev()
        .find_map(|(key, value)| (key == "PATH").then_some(value))
    else {
        return Vec::new();
    };
    let mut roots = Vec::new();
    for entry in std::env::split_paths(path) {
        push_runtime_projection(&mut roots, entry);
    }
    roots
}

pub(super) fn projected_runtime_roots_for_command(
    env: &[(String, String)],
    argv: &[String],
) -> Vec<String> {
    let mut roots = projected_runtime_roots(env);
    if let Some(executable) = argv.first().map(PathBuf::from)
        && let Some(parent) = executable.parent()
    {
        push_runtime_projection(&mut roots, parent.to_path_buf());
    }
    roots
}

/// One physical-root admission for both OS adapters. Linux additionally checks
/// the retained descriptor's actual kernel pathname; macOS emits grants for
/// the canonical target so a substituted alias does not grant its new target.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[expect(
    clippy::disallowed_methods,
    reason = "the seatbelt profile admits the runtime roots under the host HOME"
)]
pub(super) fn prepare_runtime_root(
    destination: &str,
) -> Result<Option<(PathBuf, awaken_fs_durability::ProjectionRoot)>, pc::SandboxError> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let physical_home = home
        .as_ref()
        .and_then(|home| std::fs::canonicalize(home).ok());
    let admitted = |path: &std::path::Path| {
        is_projectable_runtime_root(path, home.as_deref())
            && is_projectable_runtime_root(path, physical_home.as_deref())
    };
    let physical = match std::fs::canonicalize(destination) {
        Ok(path) => path,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(pc::SandboxError::invalid(
                "runtime projection cannot be resolved safely",
            ));
        }
    };
    if !admitted(&physical) {
        return Err(pc::SandboxError::invalid(
            "runtime projection resolves to protected host state",
        ));
    }
    let root = awaken_fs_durability::ProjectionRoot::capture(&physical)
        .map_err(|_| pc::SandboxError::invalid("runtime projection cannot be retained safely"))?;
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::{AsFd as _, AsRawFd as _};
        let opened = std::fs::read_link(format!("/proc/self/fd/{}", root.as_fd().as_raw_fd()))
            .map_err(|_| {
                pc::SandboxError::invalid("runtime projection identity cannot be observed")
            })?;
        if opened.to_string_lossy().ends_with(" (deleted)") || !admitted(&opened) {
            return Err(pc::SandboxError::invalid(
                "opened runtime projection names protected host state",
            ));
        }
    }
    root.verify(&physical)
        .map_err(|_| pc::SandboxError::invalid("runtime projection changed during admission"))?;
    Ok(Some((physical, root)))
}

fn seatbelt_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            _ => escaped.push(ch),
        }
    }
    escaped.push('"');
    escaped
}

fn seatbelt_path_filters(path: &std::path::Path) -> String {
    let quoted = seatbelt_string(&path.to_string_lossy());
    format!("(literal {quoted})(subpath {quoted})")
}

/// Render a macOS `sandbox-exec` (Seatbelt) command line. The Apple system
/// profile supplies the minimum runtime reads; user data remains deny-by-default.
#[must_use]
pub fn sandbox_exec_argv(input: &RenderInput) -> Vec<String> {
    sandbox_exec_argv_with_runtime_roots(
        input,
        &projected_runtime_roots_for_command(input.env, input.argv),
    )
}

pub(super) fn sandbox_exec_argv_with_runtime_roots(
    input: &RenderInput,
    runtime_roots: &[String],
) -> Vec<String> {
    let mut readable = vec![
        input.host_workspace.to_path_buf(),
        input.host_outputs.to_path_buf(),
    ];
    // The Hand's capability-based file tools open the Session-private mount
    // directory before addressing an individual file relative to that handle.
    // Seatbelt has no mount namespace, so grants for leaf mount paths alone do
    // not authorize opening this directory. It contains only this sandbox's
    // declared mounts; write access remains constrained below.
    if let Some(root) = input.host_workspace.parent() {
        readable.push(root.join("mnt"));
    }
    readable.extend(input.mounts.iter().map(|mount| mount.host.clone()));
    readable.extend(runtime_roots.iter().map(PathBuf::from));
    let mut writable = vec![
        input.host_workspace.to_path_buf(),
        input.host_outputs.to_path_buf(),
    ];
    writable.extend(
        input
            .mounts
            .iter()
            .filter(|mount| !mount.read_only)
            .map(|mount| mount.host.clone()),
    );
    let readonly: Vec<_> = input
        .mounts
        .iter()
        .filter(|mount| mount.read_only)
        .map(|mount| mount.host.clone())
        .collect();

    let mut profile = String::from(
        "(version 1)(deny default)(import \"system.sb\")\
         (allow process-fork)(allow process-exec)\
         (allow file-read-metadata)\
         (allow file-read* (subpath \"/private/var/select\")\
          (subpath \"/opt/homebrew\") (subpath \"/usr/local\"))",
    );
    for path in readable {
        profile.push_str("(allow file-read* ");
        profile.push_str(&seatbelt_path_filters(&path));
        profile.push(')');
    }
    for path in writable {
        profile.push_str("(allow file-write* ");
        profile.push_str(&seatbelt_path_filters(&path));
        profile.push(')');
    }
    for path in readonly {
        profile.push_str("(deny file-write* ");
        profile.push_str(&seatbelt_path_filters(&path));
        profile.push(')');
    }
    if matches!(input.network, pc::NetworkPolicy::Unrestricted) {
        profile.push_str("(allow network*)");
    }
    let mut argv = vec![s("sandbox-exec"), s("-p"), profile, s("--")];
    argv.extend(input.argv.iter().cloned());
    argv
}
