use super::ContainerPlan;

/// The sandbox paths that must stay writable under a **read-only rootfs**: the
/// Session workspace, the outputs volume the agent writes artifacts to, and a
/// scratch `/tmp`. Declared resource mounts are realized separately (as
/// binds/volumes). Pure, so every adapter renders the same writable set atop the
/// same hardening.
#[must_use]
pub fn writable_dirs(plan: &ContainerPlan) -> Vec<String> {
    let mut dirs = vec!["/workspace".to_string()];
    // OCI creates the parent of a file bind as root:root. Keep the parent of every
    // host-materialized workspace file on a Session-private writable volume so the
    // non-root Agent can later attach, rename, or detach sibling resources without
    // granting it root. Directory binds (repositories/memory) remain governed by
    // their own mount and must not be shadowed here.
    for bind in &plan.binds {
        let is_file =
            bind.content.is_some() || bind.content_bytes.is_some() || bind.secret_content.is_some();
        let parent = is_file
            .then(|| std::path::Path::new(&bind.mount_path).parent())
            .flatten()
            .and_then(std::path::Path::to_str)
            .filter(|parent| parent.starts_with("/workspace/") && *parent != "/workspace");
        if let Some(parent) = parent
            && !dirs.iter().any(|entry| entry == parent)
        {
            dirs.push(parent.to_string());
        }
    }
    for path in [plan.outputs_volume.as_str(), "/tmp"] {
        if !dirs.iter().any(|entry| entry == path) {
            dirs.push(path.to_string());
        }
    }
    dirs
}
