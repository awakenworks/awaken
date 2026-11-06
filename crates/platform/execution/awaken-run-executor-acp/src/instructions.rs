// @formal surface requirement=worker.remote_execution.remote_protocol
// @formal reason: Projects frozen instructions through the existing ACP launch boundary; vendor prompt interpretation remains an external acceptance boundary.
//! Frozen Agent instructions delivered as an ACP adapter's real system prompt.
//!
//! ACP has no portable system-instruction field, so each catalog row declares
//! the adapter-owned routes that place text into its own system prompt. The
//! selection rule is fixed here and never branches on an adapter id:
//!
//! | Awaken replaced built-in tools | Declared route      | Placement      |
//! |--------------------------------|---------------------|----------------|
//! | yes (tool aliases)             | replace             | SystemReplace  |
//! | yes                            | none                | open fails     |
//! | no                             | append              | SystemAppend   |
//! | no                             | none / unusable     | FirstPrompt    |
//!
//! Replacing is required once Awaken disables the adapter's built-in tools: the
//! adapter preset would otherwise describe tools the model cannot call and an
//! identity that contradicts the published Agent. Every other launch keeps the
//! adapter preset, because it documents the adapter's own tools, and appends.
//! `FirstPrompt` is the explicit, traced fallback to the historical first-Step
//! envelope; it is never chosen silently.

#[cfg(feature = "execution")]
use crate::{AcpLaunch, AcpLaunchIdentity, OpenError};
#[cfg(feature = "execution")]
use awaken_provisioning_contract as pc;

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

/// Where the frozen Agent instructions reach the model for one launch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg(feature = "execution")]
pub enum InstructionsPlacement {
    /// The publication has no instructions.
    Absent,
    /// The instructions replace the adapter's preset system prompt.
    SystemReplace,
    /// The instructions are appended to the adapter's preset system prompt.
    SystemAppend,
    /// The instructions ride the first Step's prompt envelope.
    FirstPrompt { reason: &'static str },
}

#[cfg(feature = "execution")]
impl InstructionsPlacement {
    /// Stable label for launch events and traces.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Absent => "absent",
            Self::SystemReplace => "system_replace",
            Self::SystemAppend => "system_append",
            Self::FirstPrompt { .. } => "first_prompt",
        }
    }

    /// Why a launch fell back to the first-Step envelope; empty otherwise.
    #[must_use]
    pub fn fallback_reason(self) -> &'static str {
        match self {
            Self::FirstPrompt { reason } => reason,
            _ => "",
        }
    }
}

/// A Session-owned instructions file the launch site must write before the
/// process starts.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg(feature = "execution")]
pub struct InstructionsFile {
    /// Path below the Session config home.
    pub relative_path: String,
    pub contents: String,
}

/// Session-owned file used when an adapter only reads instructions by path.
#[cfg(feature = "execution")]
const SESSION_INSTRUCTIONS_FILE: &str = ".awaken/agent-instructions.md";

#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "execution")]
struct PendingFile {
    channel: InstructionsChannel,
    placement: InstructionsPlacement,
    contents: String,
}

/// The resolved instruction delivery for one launch and the Session opened
/// from it.
#[derive(Debug, Clone, PartialEq)]
#[cfg(feature = "execution")]
pub struct AcpInstructions {
    placement: InstructionsPlacement,
    session_meta: Option<serde_json::Value>,
    pending_file: Option<PendingFile>,
}

#[cfg(feature = "execution")]
impl Default for AcpInstructions {
    /// Uncataloged launches (fixtures, trusted fixed ACP commands) keep the
    /// historical first-Step envelope.
    fn default() -> Self {
        Self::first_prompt("uncataloged_launch")
    }
}

#[cfg(feature = "execution")]
impl AcpInstructions {
    fn first_prompt(reason: &'static str) -> Self {
        Self {
            placement: InstructionsPlacement::FirstPrompt { reason },
            session_meta: None,
            pending_file: None,
        }
    }

    /// Final placement. A file that was never written is still pending and
    /// therefore reported as its first-prompt fallback.
    #[must_use]
    pub fn placement(&self) -> InstructionsPlacement {
        if self.pending_file.is_some() {
            InstructionsPlacement::FirstPrompt {
                reason: "session_home_unavailable",
            }
        } else {
            self.placement
        }
    }

    /// Whether the adapter already carries the instructions, so the first
    /// Step must not repeat them.
    #[must_use]
    pub fn in_system_prompt(&self) -> bool {
        matches!(
            self.placement(),
            InstructionsPlacement::SystemReplace | InstructionsPlacement::SystemAppend
        )
    }

    /// `_meta.systemPrompt` to send on every `session/new|load|resume`.
    #[must_use]
    pub fn session_meta(&self) -> Option<&serde_json::Value> {
        self.session_meta.as_ref()
    }
}

/// Enrich the existing Ready event without minting another lifecycle phase.
#[cfg(feature = "execution")]
pub(crate) struct InstructionsLaunchObserver<'a> {
    inner: crate::LaunchSink<'a>,
    detail: String,
}

#[cfg(feature = "execution")]
impl<'a> InstructionsLaunchObserver<'a> {
    pub(crate) fn new(inner: crate::LaunchSink<'a>, instructions: &AcpInstructions) -> Self {
        Self {
            inner,
            detail: format!(
                "instructions={} reason={}",
                instructions.placement().label(),
                instructions.placement().fallback_reason()
            ),
        }
    }
}

#[cfg(feature = "execution")]
impl crate::LaunchObserver for InstructionsLaunchObserver<'_> {
    fn on_launch(&self, _scope: &str, event: &crate::AcpLaunchEvent) {
        let mut event = event.clone();
        if event.stage == crate::AcpLaunchStage::Ready {
            event.detail = Some(match event.detail {
                Some(detail) => format!("{detail} {}", self.detail),
                None => self.detail.clone(),
            });
        }
        self.inner.emit(event);
    }
}

#[cfg(feature = "execution")]
impl AcpLaunch {
    /// Resolve how this launch delivers `instructions`. Must run after
    /// [`AcpLaunch::project_tool_aliases`], whose result selects the mode.
    pub fn project_instructions(
        &mut self,
        delivery: InstructionsDelivery,
        instructions: &str,
    ) -> Result<(), OpenError> {
        let text = instructions;
        if text.trim().is_empty() && self.tool_aliases.is_empty() {
            self.instructions = AcpInstructions {
                placement: InstructionsPlacement::Absent,
                session_meta: None,
                pending_file: None,
            };
            return Ok(());
        }
        let (channel, placement) = if self.tool_aliases.is_empty() {
            match delivery.append {
                Some(channel) => (channel, InstructionsPlacement::SystemAppend),
                None => {
                    self.instructions =
                        AcpInstructions::first_prompt("adapter_has_no_append_route");
                    return Ok(());
                }
            }
        } else {
            let channel = delivery.replace.ok_or_else(|| {
                OpenError(
                    "ACP_INSTRUCTIONS_UNSUPPORTED: adapter without built-in tools has no system prompt replace route"
                        .into(),
                )
            })?;
            (channel, InstructionsPlacement::SystemReplace)
        };
        let mut resolved = AcpInstructions {
            placement,
            session_meta: None,
            pending_file: None,
        };
        match channel {
            InstructionsChannel::SessionMetaSystemPrompt => {
                resolved.session_meta = Some(match placement {
                    // The pinned adapter ignores a falsy empty string. A
                    // newline selects custom-prompt mode with no semantic
                    // policy, preserving Clear without restoring the preset.
                    InstructionsPlacement::SystemReplace => serde_json::Value::String(
                        if text.trim().is_empty() { "\n" } else { text }.into(),
                    ),
                    _ => serde_json::json!({ "append": text }),
                });
            }
            InstructionsChannel::JsonConfigKey { env, key } => {
                let mut document = json_config(&self.env, env)?;
                document.insert(key.into(), serde_json::Value::String(text.into()));
                set_inline(
                    &mut self.env,
                    env,
                    serde_json::Value::Object(document).to_string(),
                );
            }
            InstructionsChannel::ProcessEnv { env } => set_inline(&mut self.env, env, text.into()),
            InstructionsChannel::HomeFile { .. }
                if self.identity == AcpLaunchIdentity::BackendOwned =>
            {
                resolved = AcpInstructions::first_prompt("backend_owned_home");
            }
            InstructionsChannel::HomeFile { .. }
                if text.lines().any(|line| line.trim_start().starts_with('@')) =>
            {
                // The global memory channel expands @path imports. Frozen
                // Agent text must not become an implicit filesystem read.
                resolved = AcpInstructions::first_prompt("instruction_file_import_syntax");
            }
            InstructionsChannel::HomeFile { .. }
            | InstructionsChannel::JsonConfigFileList { .. } => {
                resolved.pending_file = Some(PendingFile {
                    channel,
                    placement,
                    contents: text.to_owned(),
                });
            }
        }
        self.instructions = resolved;
        Ok(())
    }

    /// Bind a pending instructions file to the Session config home whose
    /// absolute, process-visible path is `home`. Returns the file the caller
    /// must write below that home before starting the process.
    pub fn bind_instructions_file(
        &mut self,
        home: &str,
    ) -> Result<Option<InstructionsFile>, OpenError> {
        let Some(pending) = self.instructions.pending_file.clone() else {
            return Ok(None);
        };
        let absolute_home = home.starts_with('/')
            || (home.as_bytes().get(1) == Some(&b':')
                && home
                    .as_bytes()
                    .get(2)
                    .is_some_and(|byte| matches!(byte, b'/' | b'\\')));
        if !absolute_home {
            return Err(OpenError("ACP_INSTRUCTIONS_UNSUPPORTED: Session home must be an absolute process-visible path".into()));
        }
        let relative_path = match pending.channel {
            InstructionsChannel::HomeFile { relative_path } => relative_path.to_string(),
            InstructionsChannel::JsonConfigFileList { env, key } => {
                let absolute =
                    format!("{}/{SESSION_INSTRUCTIONS_FILE}", home.trim_end_matches('/'));
                // OpenCode substitutes `{env:…}`/`{file:…}` in its config text.
                if absolute.contains(['{', '}']) {
                    return Err(OpenError(
                        "ACP_INSTRUCTIONS_UNSUPPORTED: Session home path contains config substitution braces"
                            .into(),
                    ));
                }
                let mut document = json_config(&self.env, env)?;
                let list = document
                    .entry(key.to_string())
                    .or_insert_with(|| serde_json::Value::Array(Vec::new()));
                let serde_json::Value::Array(entries) = list else {
                    return Err(OpenError(format!(
                        "ACP_INSTRUCTIONS_UNSUPPORTED: `{env}` key `{key}` is not a list"
                    )));
                };
                let path = serde_json::Value::String(absolute);
                if !entries.contains(&path) {
                    entries.push(path);
                }
                set_inline(
                    &mut self.env,
                    env,
                    serde_json::Value::Object(document).to_string(),
                );
                SESSION_INSTRUCTIONS_FILE.to_string()
            }
            _ => unreachable!("only file routes are pending"),
        };
        self.instructions.placement = pending.placement;
        self.instructions.pending_file = None;
        Ok(Some(InstructionsFile {
            relative_path,
            contents: pending.contents,
        }))
    }
}

/// Read the inline JSON object an adapter already receives in `env`, or start
/// an empty one. A secret or malformed value is never rewritten.
#[cfg(feature = "execution")]
fn json_config(
    env: &[pc::EnvVar],
    name: &str,
) -> Result<serde_json::Map<String, serde_json::Value>, OpenError> {
    let Some(var) = env.iter().find(|var| var.name == name) else {
        return Ok(serde_json::Map::new());
    };
    let pc::EnvValue::Inline { value } = &var.value else {
        return Err(OpenError(format!(
            "ACP_INSTRUCTIONS_UNSUPPORTED: `{name}` is not an inline configuration"
        )));
    };
    match serde_json::from_str(value) {
        Ok(serde_json::Value::Object(document)) => Ok(document),
        _ => Err(OpenError(format!(
            "ACP_INSTRUCTIONS_UNSUPPORTED: `{name}` is not a JSON object"
        ))),
    }
}

#[cfg(feature = "execution")]
fn set_inline(env: &mut Vec<pc::EnvVar>, name: &str, value: String) {
    env.retain(|var| var.name != name);
    env.push(pc::EnvVar {
        name: name.to_string(),
        value: pc::EnvValue::Inline { value },
        visibility: pc::EnvVisibility::Process,
    });
}

#[cfg(all(test, feature = "execution"))]
mod tests;
