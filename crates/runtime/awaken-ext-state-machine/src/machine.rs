//! Compiled state-machine model.
//!
//! A [`Machine`] constrains the order of tool calls. Each machine tracks one
//! *instance* per extracted [`KeyTemplate`] value (e.g. one instance per
//! `file_path`), so a constraint like "read before write" is enforced
//! per-resource rather than globally. The runtime instance state lives in
//! [`crate::state`]; this module only holds the immutable definition.

use std::path::{Component, Path, PathBuf};

use awaken_agent_contract::agent::message::Role;
use awaken_tool_pattern::{PathSegment, ToolCallPattern, resolve_path, value_to_string};
use serde_json::Value;

use crate::result::{ResultMatcher, ToolResultView, result_matches};

/// Lifetime scope of a machine's instance states.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MachineScope {
    /// Instance states persist across runs on the same thread (default).
    #[default]
    Thread,
    /// Instance states reset at the start of every run.
    Run,
}

/// What to do when a tool call is a defined transition but the current instance
/// state is not one of its `from` states (a precondition violation).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ViolationAction {
    /// Reject this single call with an error result fed back to the model.
    #[default]
    Deny,
    /// Suspend the call for external approval (human-in-the-loop).
    Ask,
    /// Allow the call but inject a warning context message after execution.
    Warn,
}

/// Normalization applied after rendering an instance key from tool arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KeyNormalizer {
    #[default]
    None,
    Trim,
    Lowercase,
    /// Lexically normalize path separators plus `.` / `..` components.
    Path,
    /// Normalize URL identity: lowercase scheme/host, drop fragments.
    Url,
}

impl KeyNormalizer {
    #[must_use]
    pub fn normalize(self, key: String) -> String {
        match self {
            KeyNormalizer::None => key,
            KeyNormalizer::Trim => key.trim().to_string(),
            KeyNormalizer::Lowercase => key.to_lowercase(),
            KeyNormalizer::Path => normalize_path_key(&key),
            KeyNormalizer::Url => normalize_url_key(&key),
        }
    }
}

/// Violation handling for a transition.
#[derive(Debug, Clone)]
pub struct Violation {
    pub action: ViolationAction,
    /// Optional reason template; `{field}` placeholders are interpolated.
    pub reason: Option<KeyTemplate>,
}

impl Default for Violation {
    fn default() -> Self {
        Self {
            action: ViolationAction::Deny,
            reason: None,
        }
    }
}

/// A single transition.
#[derive(Debug, Clone)]
pub struct Transition {
    pub pattern: ToolCallPattern,
    pub from: Vec<String>,
    pub to: String,
    /// Result condition (post-execution). `None` ⇒ success-only.
    pub when: Option<ResultMatcher>,
    /// Optional context message injected when this transition fires.
    pub emit: Option<Emit>,
    pub on_violation: Violation,
}

/// Where an emitted context message is placed in the prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmitTarget {
    /// After the base system prompt.
    System,
    /// After the conversation history (default — least intrusive).
    #[default]
    SuffixSystem,
    /// In the session-context band.
    Session,
    /// As a conversation message.
    Conversation,
}

/// A context message emitted by a transition (or a `warn` violation).
#[derive(Debug, Clone)]
pub struct Emit {
    pub target: EmitTarget,
    /// Message body; `{field}` placeholders are interpolated from tool args.
    pub content: KeyTemplate,
    /// Minimum steps between re-injections (dedup throttle).
    pub cooldown_turns: u32,
    /// Message role for the `Session` / `Conversation` targets. `None` ⇒ `User`.
    /// Ignored for the system targets, whose role is always `System`.
    pub role: Option<Role>,
}

impl Transition {
    /// Whether this transition fires for the given result. An absent `when`
    /// fires only on success.
    #[must_use]
    pub fn result_matches(&self, result: &ToolResultView<'_>) -> bool {
        match &self.when {
            None => !result.is_error,
            Some(m) => result_matches(m, result),
        }
    }

    /// Whether `state` is one of this transition's `from` states.
    #[must_use]
    pub fn allows_from(&self, state: &str) -> bool {
        self.from.iter().any(|s| s == state)
    }
}

/// A compiled state machine.
#[derive(Debug, Clone)]
pub struct Machine {
    pub name: String,
    pub scope: MachineScope,
    /// Instance-key template extracted from tool arguments. An empty template
    /// yields a single global instance (key `""`).
    pub key: KeyTemplate,
    pub key_normalizer: KeyNormalizer,
    pub initial: String,
    /// When true, a tool whose key can be extracted but matches no transition of
    /// this machine is treated as a violation.
    pub strict: bool,
    /// Fallback target state at advance time when the tool was an allowed
    /// transition but no `when` matched. `None` ⇒ stay in the current state.
    pub on_unmatched: Option<String>,
    /// States considered terminal for the continuation guard. Empty ⇒ no
    /// loop constraint.
    pub terminal_states: Vec<String>,
    pub transitions: Vec<Transition>,
}

impl Machine {
    /// Render and normalize this machine's instance key for a tool call.
    #[must_use]
    pub fn render_key(&self, tool_args: &Value) -> Option<String> {
        self.key
            .render(tool_args)
            .map(|key| self.key_normalizer.normalize(key))
    }

    /// Whether `state` is a terminal state of this machine.
    #[must_use]
    pub fn is_terminal(&self, state: &str) -> bool {
        self.terminal_states.iter().any(|s| s == state)
    }

    /// Transitions whose pattern matches the given tool call.
    pub fn matching_transitions<'a>(
        &'a self,
        tool_name: &'a str,
        tool_args: &'a Value,
    ) -> impl Iterator<Item = &'a Transition> + 'a {
        self.transitions.iter().filter(move |t| {
            awaken_tool_pattern::pattern_matches(&t.pattern, tool_name, tool_args).is_match()
        })
    }
}

fn normalize_path_key(input: &str) -> String {
    let unified = input.replace('\\', "/");
    let path = Path::new(&unified);
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
            Component::RootDir => out.push(component.as_os_str()),
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
        }
    }
    let normalized = out.to_string_lossy().replace('\\', "/");
    if normalized.is_empty() {
        ".".to_string()
    } else {
        normalized
    }
}

fn normalize_url_key(input: &str) -> String {
    let trimmed = input.trim();
    let Ok(mut url) = url::Url::parse(trimmed) else {
        return trimmed.to_string();
    };
    url.set_fragment(None);
    url.to_string()
}

// ---------------------------------------------------------------------------
// Key / reason templates
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
enum TemplatePart {
    Literal(String),
    Field(Vec<PathSegment>),
}

/// A template that renders an instance key (or a reason string) from a tool
/// call's JSON arguments. `"{file_path}"` extracts the `file_path` argument;
/// `"{a.b}"` walks nested objects; literal text is copied as-is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyTemplate {
    parts: Vec<TemplatePart>,
}

/// Error parsing a [`KeyTemplate`].
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum KeyTemplateError {
    #[error("unbalanced '{{' in template `{0}`")]
    UnbalancedOpen(String),
    #[error("unexpected '}}' in template `{0}`")]
    UnexpectedClose(String),
    #[error("empty `{{}}` placeholder in template `{0}`")]
    EmptyPlaceholder(String),
}

impl KeyTemplate {
    /// Parse a template string.
    pub fn parse(input: &str) -> Result<Self, KeyTemplateError> {
        let mut parts = Vec::new();
        let mut literal = String::new();
        let mut chars = input.chars().peekable();
        while let Some(c) = chars.next() {
            match c {
                '{' => {
                    if !literal.is_empty() {
                        parts.push(TemplatePart::Literal(std::mem::take(&mut literal)));
                    }
                    let mut path = String::new();
                    let mut closed = false;
                    for pc in chars.by_ref() {
                        if pc == '}' {
                            closed = true;
                            break;
                        }
                        path.push(pc);
                    }
                    if !closed {
                        return Err(KeyTemplateError::UnbalancedOpen(input.to_string()));
                    }
                    if path.is_empty() {
                        return Err(KeyTemplateError::EmptyPlaceholder(input.to_string()));
                    }
                    parts.push(TemplatePart::Field(parse_field_path(&path)));
                }
                '}' => return Err(KeyTemplateError::UnexpectedClose(input.to_string())),
                _ => literal.push(c),
            }
        }
        if !literal.is_empty() {
            parts.push(TemplatePart::Literal(literal));
        }
        Ok(Self { parts })
    }

    /// Whether this template has no parts (renders to `""` — a global instance).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.parts.is_empty()
    }

    /// Render the template against tool arguments. Returns `None` if any
    /// placeholder cannot be resolved.
    #[must_use]
    pub fn render(&self, args: &Value) -> Option<String> {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Literal(s) => out.push_str(s),
                TemplatePart::Field(path) => {
                    let resolved = resolve_path(args, path);
                    let first = resolved.first()?;
                    out.push_str(&value_to_string(first));
                }
            }
        }
        Some(out)
    }

    /// Render best-effort: unresolved placeholders fall back to their dotted
    /// path so a human-facing reason is still informative.
    #[must_use]
    pub fn render_lossy(&self, args: &Value) -> String {
        let mut out = String::new();
        for part in &self.parts {
            match part {
                TemplatePart::Literal(s) => out.push_str(s),
                TemplatePart::Field(path) => match resolve_path(args, path).first() {
                    Some(v) => out.push_str(&value_to_string(v)),
                    None => {
                        let dotted = path
                            .iter()
                            .map(ToString::to_string)
                            .collect::<Vec<_>>()
                            .join(".");
                        out.push_str(&format!("{{{dotted}}}"));
                    }
                },
            }
        }
        out
    }
}

fn parse_field_path(path: &str) -> Vec<PathSegment> {
    let mut segments = Vec::new();
    for raw in path.split('.') {
        let mut name = String::new();
        let mut chars = raw.chars().peekable();
        while let Some(&c) = chars.peek() {
            if c == '[' {
                break;
            }
            name.push(c);
            chars.next();
        }
        if !name.is_empty() {
            segments.push(PathSegment::Field(name));
        }
        while chars.peek() == Some(&'[') {
            chars.next();
            let mut idx = String::new();
            for ic in chars.by_ref() {
                if ic == ']' {
                    break;
                }
                idx.push(ic);
            }
            if idx == "*" {
                segments.push(PathSegment::AnyIndex);
            } else if let Ok(n) = idx.parse::<usize>() {
                segments.push(PathSegment::Index(n));
            }
        }
    }
    segments
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn template_single_field() {
        let t = KeyTemplate::parse("{file_path}").unwrap();
        assert_eq!(
            t.render(&json!({"file_path": "src/a.rs"})),
            Some("src/a.rs".to_string())
        );
    }

    #[test]
    fn template_literal_and_nested_and_index() {
        assert_eq!(
            KeyTemplate::parse("file:{file_path}")
                .unwrap()
                .render(&json!({"file_path": "x"})),
            Some("file:x".to_string())
        );
        assert_eq!(
            KeyTemplate::parse("{target.path}")
                .unwrap()
                .render(&json!({"target": {"path": "p"}})),
            Some("p".to_string())
        );
        assert_eq!(
            KeyTemplate::parse("{items[0].name}")
                .unwrap()
                .render(&json!({"items": [{"name": "n"}]})),
            Some("n".to_string())
        );
    }

    #[test]
    fn template_missing_and_empty_and_errors() {
        assert_eq!(
            KeyTemplate::parse("{file_path}")
                .unwrap()
                .render(&json!({"other": 1})),
            None
        );
        let empty = KeyTemplate::parse("").unwrap();
        assert!(empty.is_empty());
        assert_eq!(empty.render(&json!({})), Some(String::new()));
        assert!(KeyTemplate::parse("{file").is_err());
        assert!(KeyTemplate::parse("file}").is_err());
        assert!(KeyTemplate::parse("{}").is_err());
    }

    #[test]
    fn render_lossy_keeps_placeholder() {
        let t = KeyTemplate::parse("read {file_path} first").unwrap();
        assert_eq!(t.render_lossy(&json!({})), "read {file_path} first");
        assert_eq!(
            t.render_lossy(&json!({"file_path": "a.rs"})),
            "read a.rs first"
        );
    }

    #[test]
    fn normalizers() {
        assert_eq!(
            KeyNormalizer::Path.normalize("./src/../src/main.rs".to_string()),
            "src/main.rs"
        );
        assert_eq!(
            KeyNormalizer::Trim.normalize("  A  ".to_string()),
            "A".to_string()
        );
        assert_eq!(
            KeyNormalizer::Lowercase.normalize("A-b".to_string()),
            "a-b".to_string()
        );
        assert_eq!(
            KeyNormalizer::Url.normalize(" HTTPS://Example.COM:443/a/../b?x=1#frag ".to_string()),
            "https://example.com/b?x=1"
        );
        assert_eq!(
            KeyNormalizer::Url.normalize("not a url".to_string()),
            "not a url"
        );
    }

    #[test]
    fn normalize_path_boundary_cases() {
        // Collapsing to nothing yields "." (not the empty string).
        assert_eq!(KeyNormalizer::Path.normalize(String::new()), ".");
        assert_eq!(KeyNormalizer::Path.normalize(".".to_string()), ".");
        assert_eq!(KeyNormalizer::Path.normalize("src/..".to_string()), ".");
        // `..` that cannot pop past the start is preserved as a literal segment.
        assert_eq!(KeyNormalizer::Path.normalize("a/../..".to_string()), "..");
        assert_eq!(KeyNormalizer::Path.normalize("../x".to_string()), "../x");
    }

    #[test]
    fn transition_allows_from() {
        let t = Transition {
            pattern: ToolCallPattern::tool("Write"),
            from: vec!["read".into()],
            to: "written".into(),
            when: None,
            emit: None,
            on_violation: Violation::default(),
        };
        assert!(t.allows_from("read"));
        assert!(!t.allows_from("unread"));
    }
}
