//! The neutral session-mounted resource (ADR-0038). The Managed wire adapter owns
//! the `resources[]` parse form + the DTO projection; this is the crate-boundary
//! shape both sides speak.

/// One session-mounted resource (ADR-0038), parsed from a wire `resources[]` entry.
/// `kind` is the wire discriminant (`file` / `memory_store` / `github_repository`);
/// `id` is the backing reference (`file_id` / `memory_store_id` / repo `url`);
/// `mount_path` is where it appears in the sandbox; `instructions` is optional
/// per-binding guidance rendered into the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResourceAccess {
    ReadOnly,
    ReadWrite,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionResource {
    pub kind: String,
    pub id: String,
    pub mount_path: String,
    /// The maximum access granted to this binding. Realizers must preserve this
    /// value; they may narrow it, but must never widen it.
    pub access: ResourceAccess,
    pub instructions: Option<String>,
    /// `github_repository` only: the GitHub PAT the host uses to clone/push. Never
    /// echoed back and never placed in the sandbox (host-side git transport only).
    pub auth_token: Option<String>,
    /// `github_repository` only: the branch to check out (`checkout.name`); `None`
    /// clones the remote's default branch.
    pub git_ref: Option<String>,
}

/// Pure Session-control-plane composer. Runtime receives only this resolved output
/// and never reads the Agent binding repository itself.
pub struct SessionInputResolver;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInputError {
    pub mount_path: String,
}

impl std::fmt::Display for SessionInputError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "multiple resources claim mount path `{}` without one explicit Session replacement",
            self.mount_path
        )
    }
}

impl std::error::Error for SessionInputError {}

impl SessionInputResolver {
    /// Compose published Agent defaults with explicit Session attachments exactly
    /// once. A Session attachment replaces an Agent default at the same normalized
    /// mount path (Managed compatibility behavior); duplicates within either source
    /// are ambiguous and fail closed.
    pub fn resolve(
        agent_defaults: &[SessionResource],
        session_attachments: &[SessionResource],
    ) -> Result<Vec<SessionResource>, SessionInputError> {
        fn normalized(path: &str) -> &str {
            path.trim_start_matches('/')
        }

        fn unique(resources: &[SessionResource]) -> Result<(), SessionInputError> {
            let mut paths = std::collections::HashSet::new();
            for resource in resources {
                let path = normalized(&resource.mount_path);
                if !paths.insert(path) {
                    return Err(SessionInputError {
                        mount_path: resource.mount_path.clone(),
                    });
                }
            }
            Ok(())
        }

        unique(agent_defaults)?;
        unique(session_attachments)?;
        let replacements = session_attachments
            .iter()
            .map(|resource| normalized(&resource.mount_path))
            .collect::<std::collections::HashSet<_>>();
        let mut resolved = Vec::with_capacity(agent_defaults.len() + session_attachments.len());
        resolved.extend_from_slice(session_attachments);
        resolved.extend(
            agent_defaults
                .iter()
                .filter(|resource| !replacements.contains(normalized(&resource.mount_path)))
                .cloned(),
        );
        Ok(resolved)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn memory(id: &str, path: &str, access: ResourceAccess) -> SessionResource {
        SessionResource {
            kind: "memory_store".into(),
            id: id.into(),
            mount_path: path.into(),
            access,
            instructions: None,
            auth_token: None,
            git_ref: None,
        }
    }

    #[test]
    fn attachment_replaces_default_once_and_preserves_access() {
        let defaults = vec![memory("default", "/mnt/memory", ResourceAccess::ReadWrite)];
        let attachments = vec![memory("session", "mnt/memory", ResourceAccess::ReadOnly)];

        let resolved = SessionInputResolver::resolve(&defaults, &attachments).unwrap();

        assert_eq!(resolved.len(), 1);
        assert_eq!(resolved[0].id, "session");
        assert_eq!(resolved[0].access, ResourceAccess::ReadOnly);
    }

    #[test]
    fn duplicate_paths_in_one_source_fail_closed() {
        let defaults = vec![
            memory("one", "/mnt/memory", ResourceAccess::ReadOnly),
            memory("two", "mnt/memory", ResourceAccess::ReadOnly),
        ];

        let error = SessionInputResolver::resolve(&defaults, &[]).unwrap_err();

        assert_eq!(error.mount_path, "mnt/memory");
    }
}
