//! Catalog routes through which an ACP adapter accepts frozen Agent
//! instructions as its system prompt. ACP has no portable system-instruction
//! field, so each catalog row declares the adapter-owned routes; the ACP
//! executor selects one per launch and never branches on an adapter id.

/// One adapter-owned route that places text into the system prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InstructionsChannel {
    /// ACP `session/new|load|resume` `_meta.systemPrompt`: a string replaces
    /// the adapter preset and `{"append": …}` extends it.
    SessionMetaSystemPrompt,
    /// One string key inserted into the adapter's JSON configuration
    /// environment variable.
    JsonConfigKey {
        env: &'static str,
        key: &'static str,
    },
    /// A plain process environment variable the adapter adds to its prompt.
    ProcessEnv { env: &'static str },
    /// A file at this path below the adapter's home directory. Usable only
    /// when Awaken owns that home; a backend-owned login keeps the user's home.
    HomeFile { relative_path: &'static str },
    /// A Session-owned file whose absolute path is appended to a JSON array
    /// key of the adapter's configuration environment variable.
    JsonConfigFileList {
        env: &'static str,
        key: &'static str,
    },
}

/// Supported replace/append routes selected by the catalog's delivery policy.
/// `None` means this policy exposes no certified route for that mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstructionsDelivery {
    pub replace: Option<InstructionsChannel>,
    pub append: Option<InstructionsChannel>,
}
