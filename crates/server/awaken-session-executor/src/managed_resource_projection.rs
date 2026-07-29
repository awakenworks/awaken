//! Pure projection from frozen Managed resource inputs to Agent-visible paths and
//! prompt fragments. Keeping this boundary separate prevents protocol path rules
//! from being reimplemented by individual Sandbox adapters.

pub(super) fn resolved_resource_prompt(input: &awaken_protocol_managed::ResolvedInput) -> String {
    use crate::awaken_resource_contract::ResourceAccess;
    use awaken_protocol_managed::ResolvedInputSource;

    let access = match input.access {
        ResourceAccess::ReadOnly => "read-only",
        ResourceAccess::ReadWrite => "read/write",
    };
    let base = match &input.source {
        ResolvedInputSource::File { .. } => {
            let carried_path = managed_file_mount_path(&input.mount_path);
            format!("A file is mounted read-only at `{carried_path}`.")
        }
        ResolvedInputSource::MemoryStore { .. } => {
            let carried_path = format!(".mnt/{}", input.mount_path.trim_start_matches('/'));
            format!("A persistent memory store is mounted {access} at `{carried_path}`.")
        }
        ResolvedInputSource::Repository { .. } => format!(
            "A git repository is checked out at `{}` ({access}); use git there to read, edit, commit, and push.",
            input.mount_path
        ),
    };
    match &input.instructions {
        Some(instructions) if !instructions.is_empty() => format!("{base}\n{instructions}"),
        _ => base,
    }
}

pub(super) fn managed_file_mount_path(requested: &str) -> String {
    let logical = requested.trim_start_matches('/');
    if logical.starts_with("mnt/session/uploads/") {
        format!("/{logical}")
    } else {
        format!("/mnt/session/uploads/{logical}")
    }
}
