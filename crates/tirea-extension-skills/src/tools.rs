use crate::skill_md::{parse_allowed_tool_token, parse_skill_md};
use crate::{
    Skill, SkillError, SkillMaterializeError, SkillRegistry, SkillResource, SkillResourceKind,
    SkillState, SkillStateAction, SKILL_ACTIVATE_TOOL_ID, SKILL_LOAD_RESOURCE_TOOL_ID,
    SKILL_SCRIPT_TOOL_ID,
};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use std::sync::Arc;
use tirea_contract::runtime::phase::AfterToolExecuteAction;
use tirea_contract::runtime::state::AnyStateAction;
use tirea_contract::runtime::tool_call::ToolAccessGranter;
use tirea_contract::runtime::tool_call::{
    Tool, ToolCallContext, ToolDescriptor, ToolError, ToolExecutionEffect, ToolResult, ToolStatus,
};
use tirea_contract::scope::{is_scope_allowed, ScopeDomain};
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
    access_granter: Option<Arc<dyn ToolAccessGranter>>,
}

impl std::fmt::Debug for SkillActivateTool {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SkillActivateTool").finish_non_exhaustive()
    }
}

impl SkillActivateTool {
    pub fn new(registry: Arc<dyn SkillRegistry>) -> Self {
        Self {
            registry,
            access_granter: None,
        }
    }

    /// Set the tool access granter for injecting run-scoped permission overrides.
    #[must_use]
    pub fn with_access_granter(mut self, granter: Arc<dyn ToolAccessGranter>) -> Self {
        self.access_granter = Some(granter);
        self
    }

    fn resolve(&self, key: &str) -> Option<Arc<dyn Skill>> {
        self.registry.get(key.trim())
    }

    async fn execute_effect_impl(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        let key = match required_string_arg(&args, "skill") {
            Ok(v) => v,
            Err(err) => {
                return Ok(ToolExecutionEffect::from(
                    err.into_tool_result(SKILL_ACTIVATE_TOOL_ID),
                ));
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
            Err(r) => return Ok(ToolExecutionEffect::from(r)),
        };
        let meta = skill.meta();
        if !is_scope_allowed(Some(ctx.run_policy()), &meta.id, ScopeDomain::Skill) {
            return Ok(ToolExecutionEffect::from(tool_error(
                SKILL_ACTIVATE_TOOL_ID,
                "forbidden_skill",
                format!("Skill '{}' is not allowed by current policy", meta.id),
            )));
        }

        let raw = skill
            .read_instructions()
            .await
            .map_err(|e| map_skill_error(SKILL_ACTIVATE_TOOL_ID, e));
        let raw = match raw {
            Ok(v) => v,
            Err(r) => return Ok(ToolExecutionEffect::from(r)),
        };

        let doc = parse_skill_md(&raw).map_err(|e| {
            tool_error(
                SKILL_ACTIVATE_TOOL_ID,
                "invalid_skill_md",
                format!("invalid SKILL.md: {e}"),
            )
        });
        let doc = match doc {
            Ok(v) => v,
            Err(r) => return Ok(ToolExecutionEffect::from(r)),
        };
        let instructions = doc.body;
        let instruction_for_message = instructions.clone();

        let activate_action =
            AnyStateAction::new::<SkillState>(SkillStateAction::Activate(meta.id.clone()));
        let mut applied_tool_ids: Vec<String> = Vec::new();
        let mut skipped_tokens: Vec<String> = Vec::new();
        let mut seen: HashSet<String> = HashSet::new();
        let mut grant_tool_ids: Vec<String> = Vec::new();
        for token in meta.allowed_tools.iter() {
            match parse_allowed_tool_token(token.clone()) {
                Ok(parsed) if parsed.scope.is_none() => {
                    if seen.insert(parsed.tool_id.clone()) {
                        grant_tool_ids.push(parsed.tool_id.clone());
                        applied_tool_ids.push(parsed.tool_id);
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
            call_id = %ctx.call_id(),
            declared_allowed_tools = meta.allowed_tools.len(),
            applied_allowed_tools = applied_tool_ids.len(),
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

        let result = ToolResult {
            tool_name: SKILL_ACTIVATE_TOOL_ID.to_string(),
            status: ToolStatus::Success,
            data: json!({
                "activated": true,
                "skill_id": meta.id,
            }),
            message: Some(format!("Launching skill: {}", meta.id)),
            metadata: HashMap::new(),
            suspension: None,
        };

        let mut effect = ToolExecutionEffect::from(result).with_action(activate_action);
        if let Some(granter) = &self.access_granter {
            for tool_id in &grant_tool_ids {
                effect = effect.with_action(granter.grant_tool_override(tool_id));
            }
        }
        if !instruction_for_message.trim().is_empty() {
            effect = effect.with_action(AfterToolExecuteAction::AddUserMessage(
                instruction_for_message,
            ));
        }
        Ok(effect)
    }
}

#[async_trait::async_trait]
impl Tool for SkillActivateTool {
    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor::new(
            SKILL_ACTIVATE_TOOL_ID,
            "Skill",
            "Activate a skill and persist its instructions",
        )
        .with_parameters(json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Skill id or name" },
                "args": { "type": "string", "description": "Optional arguments for the skill" }
            },
            "required": ["skill"]
        }))
    }

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        Ok(self.execute_effect_impl(args, ctx).await?.result)
    }

    async fn execute_effect(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolExecutionEffect, ToolError> {
        self.execute_effect_impl(args, ctx).await
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
            "Load a skill resource file (references/** or assets/**) into persisted state",
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let tool_name = SKILL_LOAD_RESOURCE_TOOL_ID;
        let key = match required_string_arg(&args, "skill") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };
        let path = match required_string_arg(&args, "path") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };
        let kind = match parse_resource_kind(args.get("kind"), &path) {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(tool_name)),
        };

        let skill = self
            .resolve(&key)
            .ok_or_else(|| tool_error(tool_name, "unknown_skill", format!("Unknown skill: {key}")));
        let skill = match skill {
            Ok(v) => v,
            Err(r) => return Ok(r),
        };
        let meta = skill.meta();
        if !is_scope_allowed(Some(ctx.run_policy()), &meta.id, ScopeDomain::Skill) {
            return Ok(tool_error(
                tool_name,
                "forbidden_skill",
                format!("Skill '{}' is not allowed by current policy", meta.id),
            ));
        }

        let resource = skill
            .load_resource(kind, &path)
            .await
            .map_err(|e| map_skill_error(tool_name, e));
        let resource = match resource {
            Ok(v) => v,
            Err(r) => return Ok(r),
        };

        match resource {
            SkillResource::Reference(mat) => {
                debug!(
                    call_id = %ctx.call_id(),
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
                ))
            }
            SkillResource::Asset(asset) => {
                debug!(
                    call_id = %ctx.call_id(),
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
                ))
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
            "Run a skill script (scripts/**) and persist its result",
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

    async fn execute(
        &self,
        args: Value,
        ctx: &ToolCallContext<'_>,
    ) -> Result<ToolResult, ToolError> {
        let key = match required_string_arg(&args, "skill") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(SKILL_SCRIPT_TOOL_ID)),
        };
        let script = match required_string_arg(&args, "script") {
            Ok(v) => v,
            Err(err) => return Ok(err.into_tool_result(SKILL_SCRIPT_TOOL_ID)),
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
            Err(r) => return Ok(r),
        };
        let meta = skill.meta();
        if !is_scope_allowed(Some(ctx.run_policy()), &meta.id, ScopeDomain::Skill) {
            return Ok(tool_error(
                SKILL_SCRIPT_TOOL_ID,
                "forbidden_skill",
                format!("Skill '{}' is not allowed by current policy", meta.id),
            ));
        }

        let res = skill
            .run_script(&script, &argv)
            .await
            .map_err(|e| map_skill_error(SKILL_SCRIPT_TOOL_ID, e));
        let res = match res {
            Ok(v) => v,
            Err(r) => return Ok(r),
        };

        debug!(
            call_id = %ctx.call_id(),
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
        ))
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

    if let Some(expected) = parsed_kind {
        if let Some(inferred) = from_path {
            if expected != inferred {
                return Err(ToolArgError::new(
                    "invalid_arguments",
                    format!(
                        "kind '{}' does not match path prefix for '{}'",
                        expected.as_str(),
                        path
                    ),
                ));
            }
        }
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
