//! DSL parsing: a declarative machine definition (JSON/YAML) compiled to the
//! immutable [`Machine`] model.

use awaken_agent_contract::agent::message::Role;
use awaken_tool_pattern::parse_pattern;
use serde::Deserialize;

use crate::machine::{
    Emit, EmitTarget, KeyNormalizer, KeyTemplate, KeyTemplateError, Machine, MachineScope,
    Transition, Violation, ViolationAction,
};
use crate::result::{ContentMatcher, ResultMatcher, StatusMatcher};

/// The whole state-machine plugin configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct StateMachineConfig {
    #[serde(default)]
    pub machines: Vec<MachineEntry>,
    #[serde(default)]
    pub continuation: ContinuationSettings,
}

/// Loop-continuation settings shared by all machines.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ContinuationSettings {
    /// Max forced continuations while a machine instance is non-terminal. `0`
    /// disables the continuation guard.
    pub max_continuations: u32,
    /// Nudge template; `{summary}` is filled with the incomplete instances.
    pub message: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct MachineEntry {
    pub name: String,
    #[serde(default)]
    pub scope: ScopeEntry,
    #[serde(default)]
    pub key: String,
    #[serde(default)]
    pub key_normalizer: KeyNormalizerEntry,
    pub initial: String,
    #[serde(default)]
    pub strict: bool,
    #[serde(default)]
    pub on_unmatched: Option<String>,
    #[serde(default)]
    pub terminal: Vec<String>,
    #[serde(default)]
    pub transitions: Vec<TransitionEntry>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScopeEntry {
    #[default]
    Thread,
    Run,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyNormalizerEntry {
    #[default]
    None,
    Trim,
    Lowercase,
    Path,
    Url,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TransitionEntry {
    pub on: String,
    pub from: FromEntry,
    pub to: String,
    #[serde(default)]
    pub when: Option<WhenEntry>,
    #[serde(default)]
    pub emit: Option<EmitEntry>,
    #[serde(default)]
    pub on_violation: Option<ViolationEntry>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum FromEntry {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum WhenEntry {
    /// `"any"` / `"success"` / `"error"`.
    Simple(String),
    Structured {
        #[serde(default)]
        status: Option<String>,
        /// Glob over the stringified result content.
        #[serde(default)]
        content: Option<String>,
    },
}

#[derive(Debug, Clone, Deserialize)]
pub struct EmitEntry {
    #[serde(default)]
    pub target: EmitTargetEntry,
    pub content: String,
    #[serde(default)]
    pub cooldown_turns: u32,
    #[serde(default)]
    pub role: Option<RoleEntry>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmitTargetEntry {
    System,
    #[default]
    SuffixSystem,
    Session,
    Conversation,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RoleEntry {
    User,
    Assistant,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ViolationEntry {
    Action(String),
    Structured {
        action: String,
        #[serde(default)]
        reason: Option<String>,
    },
}

/// Error compiling a [`StateMachineConfig`] into machines.
#[derive(Debug, thiserror::Error)]
pub enum StateMachineConfigError {
    #[error("config parse error: {0}")]
    Parse(String),
    #[error("invalid tool pattern `{pattern}`: {message}")]
    Pattern { pattern: String, message: String },
    #[error("invalid key template: {0}")]
    Template(#[from] KeyTemplateError),
    #[error("unknown violation action `{0}`")]
    Action(String),
    #[error("unknown result status `{0}`")]
    Status(String),
    #[error("duplicate machine name `{0}`")]
    DuplicateMachine(String),
}

impl StateMachineConfig {
    /// Parse from a JSON string.
    pub fn from_json_str(content: &str) -> Result<Self, StateMachineConfigError> {
        serde_json::from_str(content).map_err(|e| StateMachineConfigError::Parse(e.to_string()))
    }

    /// Parse from a YAML string.
    pub fn from_yaml_str(content: &str) -> Result<Self, StateMachineConfigError> {
        serde_yaml::from_str(content).map_err(|e| StateMachineConfigError::Parse(e.to_string()))
    }

    /// Build from an already-deserialized JSON value (the plugin config path).
    pub fn from_value(value: serde_json::Value) -> Result<Self, StateMachineConfigError> {
        serde_json::from_value(value).map_err(|e| StateMachineConfigError::Parse(e.to_string()))
    }

    /// Compile the configuration into the immutable machine set.
    pub fn into_machines(self) -> Result<Vec<Machine>, StateMachineConfigError> {
        let mut seen = std::collections::HashSet::new();
        let mut machines = Vec::with_capacity(self.machines.len());
        for entry in self.machines {
            if !seen.insert(entry.name.clone()) {
                return Err(StateMachineConfigError::DuplicateMachine(entry.name));
            }
            machines.push(entry.compile()?);
        }
        Ok(machines)
    }
}

impl MachineEntry {
    fn compile(self) -> Result<Machine, StateMachineConfigError> {
        let key = KeyTemplate::parse(&self.key)?;
        let transitions = self
            .transitions
            .into_iter()
            .map(TransitionEntry::compile)
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Machine {
            name: self.name,
            scope: match self.scope {
                ScopeEntry::Thread => MachineScope::Thread,
                ScopeEntry::Run => MachineScope::Run,
            },
            key,
            key_normalizer: match self.key_normalizer {
                KeyNormalizerEntry::None => KeyNormalizer::None,
                KeyNormalizerEntry::Trim => KeyNormalizer::Trim,
                KeyNormalizerEntry::Lowercase => KeyNormalizer::Lowercase,
                KeyNormalizerEntry::Path => KeyNormalizer::Path,
                KeyNormalizerEntry::Url => KeyNormalizer::Url,
            },
            initial: self.initial,
            strict: self.strict,
            on_unmatched: self.on_unmatched,
            terminal_states: self.terminal,
            transitions,
        })
    }
}

impl TransitionEntry {
    fn compile(self) -> Result<Transition, StateMachineConfigError> {
        let pattern = parse_pattern(&self.on).map_err(|e| StateMachineConfigError::Pattern {
            pattern: self.on.clone(),
            message: e.to_string(),
        })?;
        let from = match self.from {
            FromEntry::One(s) => vec![s],
            FromEntry::Many(v) => v,
        };
        let when = self.when.map(compile_when).transpose()?;
        let emit = self.emit.map(compile_emit).transpose()?;
        let on_violation = self
            .on_violation
            .map(compile_violation)
            .transpose()?
            .unwrap_or_default();
        Ok(Transition {
            pattern,
            from,
            to: self.to,
            when,
            emit,
            on_violation,
        })
    }
}

fn compile_status(status: &str) -> Result<StatusMatcher, StateMachineConfigError> {
    match status {
        "any" => Ok(StatusMatcher::Any),
        "success" => Ok(StatusMatcher::Success),
        "error" => Ok(StatusMatcher::Error),
        other => Err(StateMachineConfigError::Status(other.to_string())),
    }
}

fn compile_when(when: WhenEntry) -> Result<ResultMatcher, StateMachineConfigError> {
    match when {
        WhenEntry::Simple(s) if s == "any" => Ok(ResultMatcher::Any),
        WhenEntry::Simple(s) => Ok(ResultMatcher::Status(compile_status(&s)?)),
        WhenEntry::Structured { status, content } => {
            let status = status.map(|s| compile_status(&s)).transpose()?;
            let content = content.map(|value| ContentMatcher::Text {
                op: awaken_tool_pattern::MatchOp::Glob,
                value,
            });
            Ok(match (status, content) {
                (Some(status), Some(content)) => ResultMatcher::Both { status, content },
                (Some(status), None) => ResultMatcher::Status(status),
                (None, Some(content)) => ResultMatcher::Content(content),
                (None, None) => ResultMatcher::Any,
            })
        }
    }
}

fn compile_emit(emit: EmitEntry) -> Result<Emit, StateMachineConfigError> {
    Ok(Emit {
        target: match emit.target {
            EmitTargetEntry::System => EmitTarget::System,
            EmitTargetEntry::SuffixSystem => EmitTarget::SuffixSystem,
            EmitTargetEntry::Session => EmitTarget::Session,
            EmitTargetEntry::Conversation => EmitTarget::Conversation,
        },
        content: KeyTemplate::parse(&emit.content)?,
        cooldown_turns: emit.cooldown_turns,
        role: emit.role.map(|r| match r {
            RoleEntry::User => Role::User,
            RoleEntry::Assistant => Role::Assistant,
        }),
    })
}

fn compile_violation(entry: ViolationEntry) -> Result<Violation, StateMachineConfigError> {
    let (action, reason) = match entry {
        ViolationEntry::Action(a) => (a, None),
        ViolationEntry::Structured { action, reason } => (action, reason),
    };
    let action = match action.as_str() {
        "deny" => ViolationAction::Deny,
        "ask" => ViolationAction::Ask,
        "warn" => ViolationAction::Warn,
        other => return Err(StateMachineConfigError::Action(other.to_string())),
    };
    let reason = reason.map(|r| KeyTemplate::parse(&r)).transpose()?;
    Ok(Violation { action, reason })
}

#[cfg(test)]
mod tests {
    use super::*;

    const READ_BEFORE_WRITE: &str = r#"{"machines":[{
        "name":"rbw","scope":"thread","key":"{file_path}","initial":"unread","terminal":["written"],
        "transitions":[
            {"on":"Read(file_path ~ \"*\")","from":["unread","written","read"],"to":"read"},
            {"on":"Write(file_path ~ \"*\")","from":"read","to":"written","when":{"status":"success"},
             "emit":{"target":"system","content":"Wrote {file_path}","cooldown_turns":2},
             "on_violation":{"action":"deny","reason":"Read {file_path} before writing."}}
        ]}],"continuation":{"max_continuations":25,"message":"Finish: {summary}"}}"#;

    #[test]
    fn compiles_read_before_write() {
        let cfg = StateMachineConfig::from_json_str(READ_BEFORE_WRITE).unwrap();
        assert_eq!(cfg.continuation.max_continuations, 25);
        let machines = cfg.into_machines().unwrap();
        assert_eq!(machines.len(), 1);
        let m = &machines[0];
        assert_eq!(m.scope, MachineScope::Thread);
        assert_eq!(m.terminal_states, vec!["written".to_string()]);
        assert_eq!(m.transitions.len(), 2);
        assert_eq!(m.transitions[1].on_violation.action, ViolationAction::Deny);
        assert!(m.transitions[1].emit.is_some());
    }

    #[test]
    fn yaml_and_json_agree() {
        let yaml = r#"
machines:
  - name: rbw
    key: "{file_path}"
    initial: unread
    transitions:
      - on: 'Read(file_path ~ "*")'
        from: [unread, read]
        to: read
"#;
        let m = StateMachineConfig::from_yaml_str(yaml)
            .unwrap()
            .into_machines()
            .unwrap();
        assert_eq!(m.len(), 1);
        assert_eq!(
            m[0].transitions[0].from,
            vec!["unread".to_string(), "read".to_string()]
        );
    }

    #[test]
    fn rejects_duplicate_machine_names() {
        let cfg = r#"{"machines":[
            {"name":"m","initial":"a","transitions":[]},
            {"name":"m","initial":"a","transitions":[]}]}"#;
        let err = StateMachineConfig::from_json_str(cfg)
            .unwrap()
            .into_machines()
            .unwrap_err();
        assert!(matches!(err, StateMachineConfigError::DuplicateMachine(_)));
    }

    #[test]
    fn rejects_invalid_pattern() {
        let cfg = r#"{"machines":[{"name":"m","initial":"a",
            "transitions":[{"on":"Read(","from":"a","to":"b"}]}]}"#;
        let err = StateMachineConfig::from_json_str(cfg)
            .unwrap()
            .into_machines()
            .unwrap_err();
        assert!(matches!(err, StateMachineConfigError::Pattern { .. }));
    }

    #[test]
    fn rejects_unknown_action_and_status() {
        let bad_action = r#"{"machines":[{"name":"m","initial":"a",
            "transitions":[{"on":"X","from":"a","to":"b","on_violation":"boom"}]}]}"#;
        assert!(matches!(
            StateMachineConfig::from_json_str(bad_action)
                .unwrap()
                .into_machines()
                .unwrap_err(),
            StateMachineConfigError::Action(_)
        ));
        let bad_status = r#"{"machines":[{"name":"m","initial":"a",
            "transitions":[{"on":"X","from":"a","to":"b","when":{"status":"maybe"}}]}]}"#;
        assert!(matches!(
            StateMachineConfig::from_json_str(bad_status)
                .unwrap()
                .into_machines()
                .unwrap_err(),
            StateMachineConfigError::Status(_)
        ));
    }

    #[test]
    fn compiles_every_enum_variant() {
        let cfg = r#"{"machines":[
            {"name":"a","scope":"run","key":"{p}","key_normalizer":"trim","initial":"s",
             "on_unmatched":"x","strict":true,
             "transitions":[
                {"on":"T","from":"s","to":"t","when":"any",
                 "emit":{"target":"system","content":"c1"}},
                {"on":"U","from":"s","to":"u","when":{"status":"error"},
                 "emit":{"target":"session","content":"c2","role":"user"},
                 "on_violation":{"action":"ask"}},
                {"on":"V","from":"s","to":"v","when":{"content":"*ok*"},
                 "emit":{"target":"conversation","content":"c3","role":"assistant"},
                 "on_violation":"warn"},
                {"on":"W","from":"s","to":"w",
                 "when":{"status":"success","content":"*done*"}}
             ]},
            {"name":"b","key_normalizer":"lowercase","initial":"s","transitions":[]},
            {"name":"c","key_normalizer":"path","initial":"s","transitions":[]},
            {"name":"d","key_normalizer":"url","initial":"s","transitions":[]}
        ]}"#;
        let machines = StateMachineConfig::from_json_str(cfg)
            .unwrap()
            .into_machines()
            .unwrap();
        assert_eq!(machines.len(), 4);
        let a = &machines[0];
        assert_eq!(a.scope, MachineScope::Run);
        assert!(a.strict);
        assert_eq!(a.on_unmatched.as_deref(), Some("x"));
        assert_eq!(a.transitions[1].on_violation.action, ViolationAction::Ask);
        assert_eq!(a.transitions[2].on_violation.action, ViolationAction::Warn);
        assert!(matches!(a.transitions[0].when, Some(ResultMatcher::Any)));
        assert!(matches!(
            a.transitions[3].when,
            Some(ResultMatcher::Both { .. })
        ));
    }

    #[test]
    fn from_value_and_yaml_error_paths() {
        // from_value round-trips a JSON object.
        let value = serde_json::json!({"machines":[{"name":"m","initial":"a","transitions":[]}]});
        assert_eq!(
            StateMachineConfig::from_value(value)
                .unwrap()
                .machines
                .len(),
            1
        );
        // malformed JSON / YAML are parse errors.
        assert!(matches!(
            StateMachineConfig::from_json_str("{not json").unwrap_err(),
            StateMachineConfigError::Parse(_)
        ));
        assert!(matches!(
            StateMachineConfig::from_yaml_str("machines: [ : : ]").unwrap_err(),
            StateMachineConfigError::Parse(_)
        ));
    }
}
