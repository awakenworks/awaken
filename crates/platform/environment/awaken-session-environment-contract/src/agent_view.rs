//! How an Agent sees the Session environment's filesystem.

/// Provider-relative view of a sandbox-logical path presented to an Agent.
/// `SandboxAbsolute` is valid inside Namespace/Container. `WorkdirRelative`
/// deliberately strips the provider-private host root, so a trusted local ACP
/// process receives a usable path without leaking the operator's absolute path
/// into a potentially remote model request. Consumers: the environment that
/// derives it and the Host's Resource prompt rendering.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum AgentPathView {
    #[default]
    SandboxAbsolute,
    WorkdirRelative {
        logical_cwd: String,
    },
}

impl AgentPathView {
    #[must_use]
    pub fn sandbox_absolute() -> Self {
        Self::SandboxAbsolute
    }

    #[must_use]
    pub fn workdir_relative(logical_cwd: impl Into<String>) -> Self {
        Self::WorkdirRelative {
            logical_cwd: logical_cwd.into(),
        }
    }

    /// `logical` as this view presents it, with the platform separator.
    #[must_use]
    pub fn render(&self, logical: &str) -> String {
        self.render_with_separator(logical, std::path::MAIN_SEPARATOR)
    }

    /// `logical` as this view presents it, with an explicit separator so
    /// both platform spellings have one oracle.
    #[must_use]
    pub fn render_with_separator(&self, logical: &str, separator: char) -> String {
        match self {
            Self::SandboxAbsolute => logical.to_string(),
            Self::WorkdirRelative { logical_cwd } => {
                relative_logical_path(logical_cwd, logical, separator)
            }
        }
    }

    /// The Workspace root as this view presents it.
    #[must_use]
    pub fn workspace(&self) -> String {
        match self {
            Self::SandboxAbsolute => awaken_provisioning_contract::WorkspaceLayout::ROOT.into(),
            Self::WorkdirRelative { logical_cwd } => relative_logical_path(
                logical_cwd,
                awaken_provisioning_contract::WorkspaceLayout::ROOT,
                std::path::MAIN_SEPARATOR,
            ),
        }
    }
}

/// Compute a portable relative path between two already-validated logical
/// sandbox paths. This never observes a provider-private host prefix.
fn relative_logical_path(from: &str, to: &str, separator: char) -> String {
    let from = from
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let to = to
        .trim_matches('/')
        .split('/')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    let common = from
        .iter()
        .zip(&to)
        .take_while(|(left, right)| left == right)
        .count();
    let mut result = std::iter::repeat_n("..", from.len() - common)
        .chain(to[common..].iter().copied())
        .collect::<Vec<_>>()
        .join("/");
    if result.is_empty() {
        result.push('.');
    }
    if separator != '/' {
        result = result.replace('/', &separator.to_string());
    }
    result
}

/// One derived view of the process, ACP, and prompt-visible filesystem.
///
/// `command_cwd` remains sandbox-logical so every provider consumes the same
/// command contract. `protocol_cwd` is the path visible to the ACP process.
/// `prompt_paths` deliberately omits the Workdir provider's private host root.
/// Consumers: Native and ACP Agent launch, Skill delivery, Session prompts.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AgentFilesystemView {
    pub protocol_cwd: String,
    pub command_cwd: String,
    pub prompt_paths: AgentPathView,
}

/// The component that will interpret a model-visible resource path.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentFilesystemClient {
    /// Awaken Hand tools consume the environment's logical/relative prompt view.
    NativeTools,
    /// An ACP CLI consumes paths relative to its protocol working directory on
    /// TrustedHost and sandbox-absolute paths in Namespace/Container.
    AcpProcess,
}
