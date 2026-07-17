//! The neutral session-mounted resource (ADR-0038). The Managed wire adapter owns
//! the `resources[]` parse form + the DTO projection; this is the crate-boundary
//! shape both sides speak.

/// One session-mounted resource (ADR-0038), parsed from a wire `resources[]` entry.
/// `kind` is the wire discriminant (`file` / `memory_store` / `github_repository`);
/// `id` is the backing reference (`file_id` / `memory_store_id` / repo `url`);
/// `mount_path` is where it appears in the sandbox; `instructions` is optional
/// per-binding guidance rendered into the system prompt.
#[derive(Debug, Clone)]
pub struct SessionResource {
    pub kind: String,
    pub id: String,
    pub mount_path: String,
    pub instructions: Option<String>,
    /// `github_repository` only: the GitHub PAT the host uses to clone/push. Never
    /// echoed back and never placed in the sandbox (host-side git transport only).
    pub auth_token: Option<String>,
    /// `github_repository` only: the branch to check out (`checkout.name`); `None`
    /// clones the remote's default branch.
    pub git_ref: Option<String>,
}
