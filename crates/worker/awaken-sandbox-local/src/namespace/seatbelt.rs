//! Pure macOS Seatbelt runtime and path projection.

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
        if let Some((prefix, _)) = path.split_once("/.local/share/uv/python/") {
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

fn push_runtime_projection(roots: &mut Vec<String>, entry: PathBuf) {
    let Some(root) = runtime_projection_root(entry) else {
        return;
    };
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
    let mut readable = vec![
        input.host_workspace.to_path_buf(),
        input.host_outputs.to_path_buf(),
    ];
    readable.extend(input.mounts.iter().map(|mount| mount.host.clone()));
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
