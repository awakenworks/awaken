use crate::error::{SkillError, SkillMaterializeError};
use crate::registry::SkillRegistry;
use crate::skill::{Skill, SkillResource, SkillResourceKind};
use crate::skill_md::parse_allowed_tool_token;
use crate::{SKILL_ACTIVATE_TOOL_ID, SKILL_LOAD_RESOURCE_TOOL_ID, SKILL_SCRIPT_TOOL_ID};
use awaken_contract::contract::tool::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolOutput, ToolResult, ToolStatus,
};
use awaken_contract::state::StateCommand;
use awaken_ext_deferred_tools::state::{DeferralState, DeferralStateAction};
use awaken_ext_permission::actions as permission_actions;
use awaken_ext_permission::rules::ToolPermissionBehavior;
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Component, Path};
use std::sync::Arc;
use tracing::{debug, warn};

#[derive(Debug)]
struct ToolArgError {
    code: &'static str,
    message: String,
}

type ToolArgResult<T> = Result<T, ToolArgError>;

impl ToolArgError {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn into_tool_result(self, tool_name: &str) -> ToolResult {
        tool_error(tool_name, self.code, self.message)
    }
}

#[derive(Clone)]
pub struct SkillActivateTool {
    registry: Arc<dyn SkillRegistry>,
}

impl std::fmt::Debug for SkillActivateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillActivateTool").finish_non_exhaustive()
    }
}

impl SkillActivateTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }

    fn resolve(&self, key: &str) -> Option<Arc<dyn Skill>> {
        self.registry.get(key.trim())
    }
}

#[async_trait::async_trait]
impl Tool for SkillActivateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            SKILL_ACTIVATE_TOOL_ID,
            "Skill",
            "Activate a skill for subsequent hidden prompt injection",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Skill id or name" },
                "args": {
                    "description": "Optional skill arguments.",
                    "oneOf": [{ "type": "string" }, { "type": "object" }]
                },
                "arguments": {
                    "type": "object",
                    "description": "Optional named arguments",
                    "additionalProperties": true
                }
            },
            "required": ["skill"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let key = match required_string_arg(&args, "skill") {
            Ok(v) => v,
            Err(err) => {
                return Ok(err.into_tool_result(SKILL_ACTIVATE_TOOL_ID).into());
            }
        };

        let skill = self.resolve(&key).ok_or_else(|| {
            tool_error(
                SKILL_ACTIVATE_TOOL_ID,
                "unknown_skill",
                format!("Unknown skill: {key}"),
            )
        });
        let skill = match skill {
            Ok(s) => s,
            Err(r) => return Ok(r.into()),
        };
        let meta = skill.meta();

        let activation_args = activation_args(&args);
        let activation = skill
            .activate(activation_args)
            .await
            .map_err(|e| map_skill_error(SKILL_ACTIVATE_TOOL_ID, e));
        let _activation = match activation {
            Ok(v) => v,
            Err(r) => return Ok(r.into()),
        };

        let mut applied_tool_ids: Vec<String> = Vec::new();
        let mut applied_tool_patterns: Vec<String> = Vec::new();
        let mut skipped_tokens: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        for token in meta.allowed_tools.iter() {
            match parse_allowed_tool_token(token.clone()) {
                Ok(parsed) if parsed.scope.is_none() => {
                    if seen.insert(parsed.tool_id.clone()) {
                        if is_pattern_like_tool_matcher(&parsed.tool_id) {
                            applied_tool_patterns.push(parsed.tool_id);
                        } else {
                            applied_tool_ids.push(parsed.tool_id);
                        }
                    }
                }
                Ok(parsed) => {
                    skipped_tokens.push(parsed.raw);
                }
                Err(_) => {
                    skipped_tokens.push(token.clone());
                }
            }
        }

        debug!(
            skill_id = %meta.id,
            call_id = %ctx.call_id,
            declared_allowed_tools = meta.allowed_tools.len(),
            applied_allowed_tools = applied_tool_ids.len(),
            applied_allowed_patterns = applied_tool_patterns.len(),
            skipped_allowed_tools = skipped_tokens.len(),
            "skill activated"
        );

        if !skipped_tokens.is_empty() {
            warn!(
                skill_id = %meta.id,
                skipped_tokens = ?skipped_tokens,
                "skipped scoped/unsupported allowed-tools tokens"
            );
        }

        // Build side-effects: state mutation + permission elevation
        let mut cmd = StateCommand::new();

        // 1. Track activation in SkillState (grow-only set, commutative merge)
        cmd.update::<crate::state::SkillState>(crate::state::SkillStateUpdate::Activate(
            meta.id.clone(),
        ));

        // 2. Grant run-scoped permission overrides for declared allowed_tools
        for tool_id in &applied_tool_ids {
            permission_actions::grant_tool_override(&mut cmd, tool_id);
        }
        for pattern in &applied_tool_patterns {
            permission_actions::grant_rule_override(
                &mut cmd,
                pattern,
                ToolPermissionBehavior::Allow,
            );
        }

        // 3. Promote deferred tools so the LLM can see their schemas.
        //    Skill declares allowed_tools because it intends to use them now;
        //    if any are deferred, they must become eager before next inference.
        if !applied_tool_ids.is_empty() {
            cmd.update::<DeferralState>(DeferralStateAction::PromoteBatch(
                applied_tool_ids.clone(),
            ));
        }

        let result = ToolResult {
            tool_name: SKILL_ACTIVATE_TOOL_ID.to_string(),
            status: ToolStatus::Success,
            data: json!({
                "activated": true,
                "skill_id": meta.id,
            }),
            message: Some(format!("Launching skill: {}", meta.id)),
            suspension: None,
            metadata: Default::default(),
        };

        Ok(ToolOutput::with_command(result, cmd))
    }
}

#[derive(Clone)]
pub struct LoadSkillResourceTool {
    registry: Arc<dyn SkillRegistry>,
}

impl std::fmt::Debug for LoadSkillResourceTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoadSkillResourceTool")
            .finish_non_exhaustive()
    }
}

impl LoadSkillResourceTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }

    fn resolve(&self, key: &str) -> Option<Arc<dyn Skill>> {
        self.registry.get(key.trim())
    }
}

#[async_trait::async_trait]
impl Tool for LoadSkillResourceTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            SKILL_LOAD_RESOURCE_TOOL_ID,
            "Load Skill Resource",
            "Load a skill resource file (references/** or assets/**)",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Skill id or name" },
                "path": { "type": "string", "description": "Relative path under references/** or assets/**" },
                "kind": { "type": "string", "enum": ["reference", "asset"], "description": "Optional resource kind; when omitted, inferred from path prefix" }
            },
            "required": ["skill", "path"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let tool_name = SKILL_LOAD_RESOURCE_TOOL_ID;
        let key = match required_string_arg(&args, "skill") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name).into()),
        };
        let path = match required_string_arg(&args, "path") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name).into()),
        };
        let kind = match parse_resource_kind(args.get("kind"), &path) {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name).into()),
        };

        let skill = self
            .resolve(&key)
            .ok_or_else(|| tool_error(tool_name, "unknown_skill", format!("Unknown skill: {key}")));
        let skill = match skill {
            Ok(v) => v,
            Err(r) => return Ok(r.into()),
        };
        let meta = skill.meta();

        let resource = skill
            .load_resource(kind, &path)
            .await
            .map_err(|e| map_skill_error(tool_name, e));
        let resource = match resource {
            Ok(v) => v,
            Err(r) => return Ok(r.into()),
        };

        match resource {
            SkillResource::Reference(mat) => {
                debug!(
                    call_id = %ctx.call_id,
                    skill_id = %meta.id,
                    kind = kind.as_str(),
                    path = %mat.path,
                    bytes = mat.bytes,
                    truncated = mat.truncated,
                    "loaded skill resource"
                );

                Ok(ToolResult::success(
                    tool_name,
                    json!({
                        "loaded": true,
                        "skill_id": meta.id,
                        "kind": kind.as_str(),
                        "path": mat.path,
                        "bytes": mat.bytes,
                        "truncated": mat.truncated,
                        "content": mat.content,
                    }),
                )
                .into())
            }
            SkillResource::Asset(asset) => {
                debug!(
                    call_id = %ctx.call_id,
                    skill_id = %meta.id,
                    kind = kind.as_str(),
                    path = %asset.path,
                    bytes = asset.bytes,
                    truncated = asset.truncated,
                    media_type = asset.media_type.as_deref().unwrap_or("application/octet-stream"),
                    "loaded skill resource"
                );

                Ok(ToolResult::success(
                    tool_name,
                    json!({
                        "loaded": true,
                        "skill_id": meta.id,
                        "kind": kind.as_str(),
                        "path": asset.path,
                        "bytes": asset.bytes,
                        "truncated": asset.truncated,
                        "media_type": asset.media_type,
                        "encoding": asset.encoding,
                        "content": asset.content,
                    }),
                )
                .into())
            }
        }
    }
}

#[derive(Clone)]
pub struct SkillScriptTool {
    registry: Arc<dyn SkillRegistry>,
}

impl std::fmt::Debug for SkillScriptTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillScriptTool").finish_non_exhaustive()
    }
}

impl SkillScriptTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self { registry }
    }

    fn resolve(&self, key: &str) -> Option<Arc<dyn Skill>> {
        self.registry.get(key.trim())
    }
}

#[async_trait::async_trait]
impl Tool for SkillScriptTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            SKILL_SCRIPT_TOOL_ID,
            "Skill Script",
            "Run a skill script (scripts/**) and return its result",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Skill id or name" },
                "script": { "type": "string", "description": "Relative path under scripts/** (e.g. scripts/run.sh)" },
                "args": { "type": "array", "items": { "type": "string" }, "description": "Optional script arguments" }
            },
            "required": ["skill", "script"]
        }))
    }

    async fn execute(&self, args: Value, ctx: &ToolCallContext) -> Result<ToolOutput, ToolError> {
        let key = match required_string_arg(&args, "skill") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(SKILL_SCRIPT_TOOL_ID).into()),
        };
        let script = match required_string_arg(&args, "script") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(SKILL_SCRIPT_TOOL_ID).into()),
        };
        let argv: Vec<String> = args
            .get("args")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        let skill = self.resolve(&key).ok_or_else(|| {
            tool_error(
                SKILL_SCRIPT_TOOL_ID,
                "unknown_skill",
                format!("Unknown skill: {key}"),
            )
        });
        let skill = match skill {
            Ok(v) => v,
            Err(r) => return Ok(r.into()),
        };
        let meta = skill.meta();

        let res = skill
            .run_script(&script, &argv)
            .await
            .map_err(|e| map_skill_error(SKILL_SCRIPT_TOOL_ID, e));
        let res = match res {
            Ok(v) => v,
            Err(r) => return Ok(r.into()),
        };

        debug!(
            call_id = %ctx.call_id,
            skill_id = %meta.id,
            script = %res.script,
            exit_code = res.exit_code,
            stdout_truncated = res.truncated_stdout,
            stderr_truncated = res.truncated_stderr,
            "executed skill script"
        );

        Ok(ToolResult::success(
            SKILL_SCRIPT_TOOL_ID,
            json!({
                "ok": res.exit_code == 0,
                "skill_id": meta.id,
                "script": res.script,
                "exit_code": res.exit_code,
                "stdout_truncated": res.truncated_stdout,
                "stderr_truncated": res.truncated_stderr,
                "stdout": res.stdout,
                "stderr": res.stderr,
            }),
        )
        .into())
    }
}

fn required_string_arg(args: &Value, key: &str) -> ToolArgResult<String> {
    let value = args.get(key).and_then(|v| v.as_str()).map(str::trim);
    match value {
        Some(v) if !v.is_empty() => Ok(v.to_string()),
        _ => Err(ToolArgError::new(
            "invalid_arguments",
            format!("missing '{key}'"),
        )),
    }
}

fn activation_args(args: &Value) -> Option<&Value> {
    args.get("arguments").or_else(|| args.get("args"))
}

fn is_pattern_like_tool_matcher(tool_id: &str) -> bool {
    tool_id.contains('*')
        || tool_id.contains('?')
        || tool_id.contains('[')
        || (tool_id.starts_with('/') && tool_id.ends_with('/') && tool_id.len() >= 2)
}

fn parse_resource_kind(kind: Option<&Value>, path: &str) -> ToolArgResult<SkillResourceKind> {
    let from_kind = kind.and_then(|v| v.as_str()).map(str::trim);

    if is_obviously_invalid_relative_path(path) {
        return Err(ToolArgError::new("invalid_path", "invalid relative path"));
    }

    let from_path = if path.starts_with("references/") {
        Some(SkillResourceKind::Reference)
    } else if path.starts_with("assets/") {
        Some(SkillResourceKind::Asset)
    } else {
        None
    };

    let parsed_kind = match from_kind {
        Some("reference") => Some(SkillResourceKind::Reference),
        Some("asset") => Some(SkillResourceKind::Asset),
        Some(other) => {
            return Err(ToolArgError::new(
                "invalid_arguments",
                format!("invalid 'kind': {other}"),
            ));
        }
        None => None,
    };

    let Some(kind) = parsed_kind.or(from_path) else {
        return Err(ToolArgError::new(
            "unsupported_path",
            "path must start with 'references/' or 'assets/'",
        ));
    };

    if let Some(expected) = parsed_kind
        && let Some(inferred) = from_path
        && expected != inferred
    {
        return Err(ToolArgError::new(
            "invalid_arguments",
            format!(
                "kind '{}' does not match path prefix for '{}'",
                expected.as_str(),
                path
            ),
        ));
    }

    Ok(kind)
}

fn is_obviously_invalid_relative_path(path: &str) -> bool {
    if path.trim().is_empty() {
        return true;
    }

    let p = Path::new(path);
    if p.is_absolute() {
        return true;
    }

    p.components().any(|c| {
        matches!(
            c,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    })
}

fn tool_error(tool_name: &str, code: &str, message: impl Into<String>) -> ToolResult {
    ToolResult::error_with_code(tool_name, code, message)
}

fn map_skill_error(tool_name: &str, e: SkillError) -> ToolResult {
    match e {
        SkillError::UnknownSkill(id) => {
            tool_error(tool_name, "unknown_skill", format!("Unknown skill: {id}"))
        }
        SkillError::InvalidSkillMd(msg) => tool_error(tool_name, "invalid_skill_md", msg),
        SkillError::InvalidArguments(msg) => tool_error(tool_name, "invalid_arguments", msg),
        SkillError::Materialize(err) => match err {
            SkillMaterializeError::InvalidPath(msg) => tool_error(tool_name, "invalid_path", msg),
            SkillMaterializeError::PathEscapesRoot => {
                tool_error(tool_name, "path_escapes_root", "path is outside skill root")
            }
            SkillMaterializeError::UnsupportedPath(msg) => tool_error(
                tool_name,
                "unsupported_path",
                format!("expected under {msg}"),
            ),
            SkillMaterializeError::Io(msg) => tool_error(tool_name, "io_error", msg),
            SkillMaterializeError::UnsupportedRuntime(msg) => {
                tool_error(tool_name, "unsupported_runtime", msg)
            }
            SkillMaterializeError::Timeout(secs) => tool_error(
                tool_name,
                "script_timeout",
                format!("script timed out after {secs}s"),
            ),
            SkillMaterializeError::InvalidScriptArgs(msg) => {
                tool_error(tool_name, "invalid_arguments", msg)
            }
        },
        SkillError::Io(msg) => tool_error(tool_name, "io_error", msg),
        SkillError::DuplicateSkillId(id) => tool_error(
            tool_name,
            "duplicate_skill_id",
            format!("duplicate skill id: {id}"),
        ),
        SkillError::Unsupported(msg) => tool_error(tool_name, "unsupported_operation", msg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::InMemorySkillRegistry;
    use crate::skill::{ScriptResult, SkillActivation, SkillMeta};
    use async_trait::async_trait;
    use std::sync::Mutex;

    #[derive(Debug)]
    struct RecordingSkill {
        meta: SkillMeta,
        seen_args: Mutex<Vec<Option<Value>>>,
    }

    #[async_trait]
    impl Skill for RecordingSkill {
        fn meta(&self) -> &SkillMeta {
            &self.meta
        }

        async fn read_instructions(&self) -> Result<String, SkillError> {
            Ok("---\nname: record\ndescription: record\n---\nunused\n".to_string())
        }

        async fn activate(&self, args: Option<&Value>) -> Result<SkillActivation, SkillError> {
            self.seen_args.lock().unwrap().push(args.cloned());
            Ok(SkillActivation {
                instructions: "Activated from custom skill".to_string(),
            })
        }

        async fn load_resource(
            &self,
            _kind: SkillResourceKind,
            _path: &str,
        ) -> Result<SkillResource, SkillError> {
            Err(SkillError::Unsupported("mock".to_string()))
        }

        async fn run_script(
            &self,
            _script: &str,
            _args: &[String],
        ) -> Result<ScriptResult, SkillError> {
            Err(SkillError::Unsupported("mock".to_string()))
        }
    }

    #[tokio::test]
    async fn skill_activate_tool_forwards_named_activation_arguments() {
        let skill = Arc::new(RecordingSkill {
            meta: SkillMeta::new("record", "record", "record", Vec::new()),
            seen_args: Mutex::new(Vec::new()),
        });
        let registry = Arc::new(InMemorySkillRegistry::from_skills(vec![
            skill.clone() as Arc<dyn Skill>
        ]));
        let tool = SkillActivateTool::new(registry);

        let ctx = ToolCallContext::test_default();
        let result = tool
            .execute(
                json!({
                    "skill": "record",
                    "arguments": { "path": "src/lib.rs" }
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(result.result.is_success());
        let calls = skill.seen_args.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0], Some(json!({"path": "src/lib.rs"})));
    }

    #[tokio::test]
    async fn skill_activate_unknown_skill_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillActivateTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "nonexistent"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
        assert!(
            result
                .result
                .message
                .as_deref()
                .unwrap_or("")
                .contains("unknown_skill")
        );
    }

    #[tokio::test]
    async fn skill_activate_missing_skill_arg_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillActivateTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool.execute(json!({}), &ctx).await.unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn load_resource_unknown_skill_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "no", "path": "references/a.md"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn script_tool_unknown_skill_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillScriptTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "no", "script": "scripts/run.sh"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn load_resource_invalid_path_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "s", "path": "../escape"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn load_resource_unsupported_path_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "s", "path": "other/file.md"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    // ── SkillActivateTool descriptor ──

    #[test]
    fn skill_activate_descriptor_has_required_skill_field() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillActivateTool::new(registry);
        let desc = tool.descriptor();
        assert_eq!(desc.id, SKILL_ACTIVATE_TOOL_ID);
        let required = desc.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v: &Value| v.as_str() == Some("skill")));
    }

    #[tokio::test]
    async fn skill_activate_empty_skill_id_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillActivateTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool.execute(json!({"skill": ""}), &ctx).await.unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn skill_activate_whitespace_only_skill_id_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillActivateTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool.execute(json!({"skill": "   "}), &ctx).await.unwrap();
        assert!(result.result.is_error());
    }

    // ── LoadSkillResourceTool descriptor ──

    #[test]
    fn load_resource_descriptor_requires_skill_and_path() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let desc = tool.descriptor();
        assert_eq!(desc.id, SKILL_LOAD_RESOURCE_TOOL_ID);
        let required = desc.parameters["required"].as_array().unwrap().clone();
        let names: Vec<&str> = required.iter().filter_map(|v: &Value| v.as_str()).collect();
        assert!(names.contains(&"skill"));
        assert!(names.contains(&"path"));
    }

    #[tokio::test]
    async fn load_resource_missing_path_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool.execute(json!({"skill": "s"}), &ctx).await.unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn load_resource_absolute_path_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "s", "path": "/etc/passwd"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn load_resource_kind_mismatch_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(
                json!({"skill": "s", "path": "references/a.md", "kind": "asset"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn load_resource_valid_skill_returns_skill_error() {
        // A valid skill that returns Unsupported for load_resource
        let skill = Arc::new(RecordingSkill {
            meta: SkillMeta::new("record", "record", "record", Vec::new()),
            seen_args: Mutex::new(Vec::new()),
        });
        let registry = Arc::new(InMemorySkillRegistry::from_skills(vec![
            skill as Arc<dyn Skill>,
        ]));
        let tool = LoadSkillResourceTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "record", "path": "references/a.md"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    // ── SkillScriptTool descriptor ──

    #[test]
    fn script_tool_descriptor_requires_skill_and_script() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillScriptTool::new(registry);
        let desc = tool.descriptor();
        assert_eq!(desc.id, SKILL_SCRIPT_TOOL_ID);
        let required = desc.parameters["required"].as_array().unwrap().clone();
        let names: Vec<&str> = required.iter().filter_map(|v: &Value| v.as_str()).collect();
        assert!(names.contains(&"skill"));
        assert!(names.contains(&"script"));
    }

    #[tokio::test]
    async fn script_tool_missing_script_returns_error() {
        let registry = Arc::new(InMemorySkillRegistry::new());
        let tool = SkillScriptTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool.execute(json!({"skill": "s"}), &ctx).await.unwrap();
        assert!(result.result.is_error());
    }

    #[tokio::test]
    async fn script_tool_valid_skill_unsupported_returns_error() {
        let skill = Arc::new(RecordingSkill {
            meta: SkillMeta::new("record", "record", "record", Vec::new()),
            seen_args: Mutex::new(Vec::new()),
        });
        let registry = Arc::new(InMemorySkillRegistry::from_skills(vec![
            skill as Arc<dyn Skill>,
        ]));
        let tool = SkillScriptTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(json!({"skill": "record", "script": "scripts/run.sh"}), &ctx)
            .await
            .unwrap();
        assert!(result.result.is_error());
    }

    // ── Script tool with actual output ──

    #[tokio::test]
    async fn script_tool_success_returns_ok_data() {
        #[derive(Debug)]
        struct ScriptSkill(SkillMeta);

        #[async_trait]
        impl Skill for ScriptSkill {
            fn meta(&self) -> &SkillMeta {
                &self.0
            }
            async fn read_instructions(&self) -> Result<String, crate::error::SkillError> {
                Ok(String::new())
            }
            async fn activate(
                &self,
                _args: Option<&Value>,
            ) -> Result<crate::skill::SkillActivation, crate::error::SkillError> {
                Ok(crate::skill::SkillActivation {
                    instructions: "ok".to_string(),
                })
            }
            async fn load_resource(
                &self,
                _kind: SkillResourceKind,
                _path: &str,
            ) -> Result<SkillResource, crate::error::SkillError> {
                Err(crate::error::SkillError::Unsupported("mock".into()))
            }
            async fn run_script(
                &self,
                script: &str,
                _args: &[String],
            ) -> Result<ScriptResult, crate::error::SkillError> {
                Ok(ScriptResult {
                    skill: "script-skill".to_string(),
                    script: script.to_string(),
                    sha256: "abc".to_string(),
                    truncated_stdout: false,
                    truncated_stderr: false,
                    exit_code: 0,
                    stdout: "hello world".to_string(),
                    stderr: String::new(),
                })
            }
        }

        let skill = Arc::new(ScriptSkill(SkillMeta::new(
            "script-skill",
            "script-skill",
            "runs scripts",
            Vec::new(),
        )));
        let registry = Arc::new(InMemorySkillRegistry::from_skills(vec![
            skill as Arc<dyn Skill>,
        ]));
        let tool = SkillScriptTool::new(registry);
        let ctx = ToolCallContext::test_default();

        let result = tool
            .execute(
                json!({"skill": "script-skill", "script": "scripts/run.sh"}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(result.result.is_success());
        assert_eq!(result.result.data["ok"], true);
        assert_eq!(result.result.data["stdout"], "hello world");
        assert_eq!(result.result.data["exit_code"], 0);
    }

    // ── Helper function tests ──

    #[test]
    fn required_string_arg_returns_err_for_null() {
        let args = json!({"key": null});
        assert!(required_string_arg(&args, "key").is_err());
    }

    #[test]
    fn required_string_arg_returns_err_for_empty_string() {
        let args = json!({"key": ""});
        assert!(required_string_arg(&args, "key").is_err());
    }

    #[test]
    fn required_string_arg_trims_whitespace() {
        let args = json!({"key": "  hello  "});
        let result = required_string_arg(&args, "key").unwrap();
        assert_eq!(result, "hello");
    }

    #[test]
    fn activation_args_prefers_arguments_over_args() {
        let args = json!({"arguments": {"a": 1}, "args": "fallback"});
        let result = activation_args(&args);
        assert_eq!(result, Some(&json!({"a": 1})));
    }

    #[test]
    fn activation_args_falls_back_to_args() {
        let args = json!({"args": "fallback"});
        let result = activation_args(&args);
        assert_eq!(result, Some(&json!("fallback")));
    }

    #[test]
    fn activation_args_returns_none_when_absent() {
        let args = json!({"skill": "s"});
        assert!(activation_args(&args).is_none());
    }

    #[test]
    fn is_pattern_like_recognizes_wildcards() {
        assert!(is_pattern_like_tool_matcher("mcp__*"));
        assert!(is_pattern_like_tool_matcher("foo?"));
        assert!(is_pattern_like_tool_matcher("[abc]"));
        assert!(is_pattern_like_tool_matcher("/regex/"));
        assert!(!is_pattern_like_tool_matcher("plain_tool_id"));
    }

    #[test]
    fn parse_resource_kind_from_path_prefix() {
        let kind = parse_resource_kind(None, "references/a.md").unwrap();
        assert_eq!(kind, SkillResourceKind::Reference);

        let kind = parse_resource_kind(None, "assets/logo.png").unwrap();
        assert_eq!(kind, SkillResourceKind::Asset);
    }

    #[test]
    fn parse_resource_kind_rejects_empty_path() {
        assert!(parse_resource_kind(None, "").is_err());
        assert!(parse_resource_kind(None, "   ").is_err());
    }

    #[test]
    fn parse_resource_kind_rejects_invalid_kind_string() {
        let kind_val = json!("invalid");
        assert!(parse_resource_kind(Some(&kind_val), "references/a.md").is_err());
    }

    #[test]
    fn is_obviously_invalid_relative_path_catches_traversal() {
        assert!(is_obviously_invalid_relative_path("../etc/passwd"));
        assert!(is_obviously_invalid_relative_path("/absolute"));
        assert!(is_obviously_invalid_relative_path(""));
        assert!(!is_obviously_invalid_relative_path("references/a.md"));
    }

    #[test]
    fn map_skill_error_covers_all_variants() {
        let e = SkillError::UnknownSkill("x".into());
        let r = map_skill_error("t", e);
        assert!(r.is_error());

        let e = SkillError::InvalidSkillMd("bad".into());
        let r = map_skill_error("t", e);
        assert!(r.is_error());

        let e = SkillError::Materialize(crate::error::SkillMaterializeError::Timeout(30));
        let r = map_skill_error("t", e);
        assert!(r.is_error());
        assert!(r.data.to_string().contains("script_timeout"));

        let e = SkillError::Io("io".into());
        let r = map_skill_error("t", e);
        assert!(r.is_error());

        let e = SkillError::DuplicateSkillId("dup".into());
        let r = map_skill_error("t", e);
        assert!(r.is_error());

        let e = SkillError::Unsupported("nope".into());
        let r = map_skill_error("t", e);
        assert!(r.is_error());
    }
}
