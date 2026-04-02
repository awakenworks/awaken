//! MCP prompt-to-skill bridge.
//!
//! Wraps MCP prompts as [`Skill`] instances via [`McpPromptSkillRegistryManager`],
//! enabling MCP-backed prompt execution through the skill subsystem.

use crate::materialize::{materialize_asset_bytes, materialize_reference_bytes};
use crate::registry::SkillRegistry;
use crate::{
    ScriptResult, Skill, SkillActivation, SkillError, SkillMeta, SkillRegistryError,
    SkillRegistryManagerError, SkillResource, SkillResourceKind,
};
use async_trait::async_trait;
use awaken_contract::PeriodicRefresher;
use awaken_ext_mcp::{
    McpError, McpPromptArgument, McpPromptEntry, McpPromptResult, McpResourceEntry,
    McpToolRegistryManager,
};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::Duration;

fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    match lock.read() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    match lock.write() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn is_prompt_support_error(err: &McpError) -> bool {
    matches!(err, McpError::Transport(msg) if msg.contains("list_prompts not supported"))
}

fn map_mcp_error(err: McpError) -> SkillError {
    match err {
        McpError::UnsupportedCapability { .. } => SkillError::Unsupported(err.to_string()),
        other => SkillError::Io(other.to_string()),
    }
}

fn enrich_description(
    base_description: &str,
    prompt_arguments: &[McpPromptArgument],
    resources: &[McpResourceHint],
) -> String {
    let mut description = base_description.trim().to_string();

    if !prompt_arguments.is_empty() {
        let args = prompt_arguments
            .iter()
            .map(|arg| {
                if arg.required {
                    format!("{} (required)", arg.name)
                } else {
                    arg.name.clone()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        description.push_str(&format!(" Args: {args}."));
    }

    if !resources.is_empty() {
        let preview = resources
            .iter()
            .take(3)
            .map(McpResourceHint::summary)
            .collect::<Vec<_>>()
            .join(", ");
        description.push_str(&format!(" Resources: {preview}"));
        if resources.len() > 3 {
            description.push_str(&format!(", +{} more", resources.len() - 3));
        }
        description.push('.');
    }

    description
}

fn mcp_skill_id(server_name: &str, prompt_name: &str) -> String {
    format!("mcp:{server_name}:{prompt_name}")
}

fn sanitize_mcp_tool_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut prev_underscore = false;
    for ch in raw.chars() {
        let keep = ch.is_ascii_alphanumeric();
        let next = if keep { ch } else { '_' };
        if next == '_' {
            if prev_underscore {
                continue;
            }
            prev_underscore = true;
        } else {
            prev_underscore = false;
        }
        out.push(next);
    }
    out.trim_matches('_').to_string()
}

#[derive(Debug, Clone)]
struct McpResourceHint {
    uri: String,
    title: Option<String>,
    description: Option<String>,
    mime_type: Option<String>,
    size: Option<u64>,
}

impl McpResourceHint {
    fn from_entry(entry: McpResourceEntry) -> Self {
        Self {
            uri: entry.resource.uri,
            title: entry.resource.title,
            description: entry.resource.description,
            mime_type: entry.resource.mime_type,
            size: entry.resource.size,
        }
    }

    fn summary(&self) -> String {
        let mut out = self.uri.clone();
        if let Some(title) = self
            .title
            .as_deref()
            .filter(|title| !title.trim().is_empty())
        {
            out.push_str(&format!(" ({title})"));
        }
        if let Some(description) = self
            .description
            .as_deref()
            .filter(|description| !description.trim().is_empty())
        {
            out.push_str(&format!(" - {description}"));
        }
        let metadata = self.metadata_summary();
        if !metadata.is_empty() {
            out.push_str(&format!(" [{metadata}]"));
        }
        out
    }

    fn metadata_summary(&self) -> String {
        let mut parts = Vec::with_capacity(2);
        if let Some(mime_type) = self
            .mime_type
            .as_deref()
            .filter(|mime_type| !mime_type.trim().is_empty())
        {
            parts.push(mime_type.to_string());
        }
        if let Some(size) = self.size {
            parts.push(format_resource_size(size));
        }
        parts.join("; ")
    }
}

fn format_resource_size(size: u64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = KIB * 1024.0;
    const GIB: f64 = MIB * 1024.0;

    match size {
        0..=1023 => format!("{size} B"),
        1024..=1_048_575 => format!("{:.1} KiB", size as f64 / KIB),
        1_048_576..=1_073_741_823 => format!("{:.1} MiB", size as f64 / MIB),
        _ => format!("{:.1} GiB", size as f64 / GIB),
    }
}

/// A skill backed by an MCP prompt.
#[derive(Debug, Clone)]
pub struct McpPromptSkill {
    meta: SkillMeta,
    server_name: String,
    prompt_name: String,
    prompt_arguments: Vec<McpPromptArgument>,
    resources: Vec<McpResourceHint>,
    manager: Arc<McpToolRegistryManager>,
}

impl McpPromptSkill {
    fn from_entry(
        manager: Arc<McpToolRegistryManager>,
        entry: McpPromptEntry,
        resources: Vec<McpResourceHint>,
    ) -> Self {
        let prompt = entry.prompt;
        let skill_id = mcp_skill_id(&entry.server_name, &prompt.name);
        let display_name = prompt.title.clone().unwrap_or_else(|| prompt.name.clone());
        let base_description = prompt.description.clone().unwrap_or_else(|| {
            format!(
                "MCP prompt '{}' from server '{}'",
                prompt.name, entry.server_name
            )
        });
        let description = enrich_description(&base_description, &prompt.arguments, &resources);
        let server_tool_pattern = format!(
            "mcp__{}__*",
            sanitize_mcp_tool_component(&entry.server_name)
        );

        Self {
            meta: SkillMeta::new(
                skill_id,
                display_name,
                description,
                vec![server_tool_pattern],
            ),
            server_name: entry.server_name,
            prompt_name: prompt.name,
            prompt_arguments: prompt.arguments,
            resources,
            manager,
        }
    }

    fn prompt_arguments_text(&self) -> String {
        if self.prompt_arguments.is_empty() {
            return "This MCP-backed skill does not declare prompt arguments.".to_string();
        }

        let mut out = String::from("Prompt arguments:\n");
        for arg in &self.prompt_arguments {
            let required = if arg.required { "required" } else { "optional" };
            let description = arg
                .description
                .as_deref()
                .unwrap_or("No description provided.");
            out.push_str(&format!("- {} ({required}): {description}\n", arg.name));
        }
        out
    }

    fn resource_text(&self) -> String {
        if self.resources.is_empty() {
            return "No MCP resources are currently advertised for this server.".to_string();
        }

        let mut out = String::from("Available MCP resources:\n");
        for resource in &self.resources {
            out.push_str(&format!("- {}\n", resource.summary()));
        }
        out
    }

    fn stringify_argument_value(value: &Value) -> Result<String, SkillError> {
        match value {
            Value::String(v) => Ok(v.clone()),
            Value::Bool(v) => Ok(v.to_string()),
            Value::Number(v) => Ok(v.to_string()),
            Value::Null => Err(SkillError::InvalidArguments(
                "null is not a valid MCP prompt argument value".to_string(),
            )),
            Value::Array(_) | Value::Object(_) => Err(SkillError::InvalidArguments(
                "MCP prompt argument values must be scalar strings, numbers, or booleans"
                    .to_string(),
            )),
        }
    }

    fn activation_arguments(
        &self,
        args: Option<&Value>,
    ) -> Result<Option<HashMap<String, String>>, SkillError> {
        let Some(args) = args else {
            return Ok(None);
        };

        match args {
            Value::Null => Ok(None),
            Value::String(raw) => {
                let raw = raw.trim();
                if raw.is_empty() {
                    return Ok(None);
                }
                match self.prompt_arguments.as_slice() {
                    [arg] => Ok(Some(HashMap::from([(arg.name.clone(), raw.to_string())]))),
                    [] => Err(SkillError::InvalidArguments(format!(
                        "skill '{}' does not accept activation args",
                        self.meta.id
                    ))),
                    _ => Err(SkillError::InvalidArguments(format!(
                        "skill '{}' requires named arguments; pass an object via 'arguments'",
                        self.meta.id
                    ))),
                }
            }
            Value::Object(map) => {
                let mut out = HashMap::with_capacity(map.len());
                for (key, value) in map {
                    out.insert(key.clone(), Self::stringify_argument_value(value)?);
                }
                if out.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(out))
                }
            }
            _ => Err(SkillError::InvalidArguments(
                "skill activation args must be a string or object".to_string(),
            )),
        }
    }

    fn render_prompt_result(&self, prompt: McpPromptResult) -> String {
        if prompt.messages.len() == 1
            && prompt.messages[0].role.eq_ignore_ascii_case("user")
            && let Some(text) = prompt.messages[0]
                .content
                .get("text")
                .and_then(Value::as_str)
        {
            return text.to_string();
        }

        let mut out = String::new();
        out.push_str(&format!(
            "<mcp_skill_prompt server=\"{}\" prompt=\"{}\">\n",
            self.server_name, self.prompt_name
        ));
        if let Some(description) = prompt.description.as_deref() {
            out.push_str(&format!("<description>{description}</description>\n"));
        }
        for message in prompt.messages {
            out.push_str(&format!("<message role=\"{}\">\n", message.role));
            out.push_str(&render_prompt_content(&message.content));
            if !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str("</message>\n");
        }
        out.push_str("</mcp_skill_prompt>");
        out
    }
}

fn render_prompt_content(content: &Value) -> String {
    if let Some(text) = content.as_str() {
        return text.to_string();
    }
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return text.to_string();
    }
    serde_json::to_string_pretty(content).unwrap_or_else(|_| content.to_string())
}

fn resource_uri_from_path(kind: SkillResourceKind, path: &str) -> Result<&str, SkillError> {
    let prefix = match kind {
        SkillResourceKind::Reference => "references/",
        SkillResourceKind::Asset => "assets/",
    };
    let uri = path.strip_prefix(prefix).ok_or_else(|| {
        SkillError::InvalidArguments(format!("resource path must start with '{prefix}'"))
    })?;
    if uri.trim().is_empty() {
        return Err(SkillError::InvalidArguments(
            "resource uri must be non-empty".to_string(),
        ));
    }
    Ok(uri)
}

fn resource_contents<'a>(value: &'a Value, uri: &str) -> Result<Vec<&'a Value>, SkillError> {
    let contents = value
        .get("contents")
        .and_then(Value::as_array)
        .ok_or_else(|| SkillError::Io(format!("MCP resource '{uri}' returned no contents")))?;
    if contents.is_empty() {
        return Err(SkillError::Io(format!(
            "MCP resource '{uri}' returned no contents"
        )));
    }

    let mut matched = Vec::new();
    let mut untagged = Vec::new();
    let mut tagged_other_uri = false;

    for content in contents {
        match content.get("uri").and_then(Value::as_str) {
            Some(content_uri) if content_uri == uri => matched.push(content),
            Some(_) => tagged_other_uri = true,
            None => untagged.push(content),
        }
    }

    if !matched.is_empty() {
        matched.extend(untagged);
        return Ok(matched);
    }

    if !untagged.is_empty() {
        return Ok(untagged);
    }

    if tagged_other_uri {
        return Err(SkillError::Io(format!(
            "MCP resource '{uri}' returned contents for a different uri"
        )));
    }

    Err(SkillError::Io(format!(
        "MCP resource '{uri}' returned no contents"
    )))
}

fn read_text_resource(value: &Value, uri: &str) -> Result<String, SkillError> {
    let contents = resource_contents(value, uri)?;
    let mut parts = Vec::new();

    for content in contents {
        if let Some(text) = content.get("text").and_then(Value::as_str) {
            parts.push(text);
            continue;
        }
        if content.get("blob").and_then(Value::as_str).is_some() {
            return Err(SkillError::Unsupported(format!(
                "MCP resource '{uri}' contains binary content; load it as an asset instead"
            )));
        }
        return Err(SkillError::Io(format!(
            "MCP resource '{uri}' does not contain text content"
        )));
    }

    if parts.is_empty() {
        return Err(SkillError::Io(format!(
            "MCP resource '{uri}' does not contain text content"
        )));
    }

    Ok(parts.join("\n\n"))
}

fn read_asset_resource(value: &Value, uri: &str) -> Result<(Vec<u8>, Option<String>), SkillError> {
    let contents = resource_contents(value, uri)?;
    if contents.len() != 1 {
        return Err(SkillError::Unsupported(format!(
            "MCP resource '{uri}' returned multiple content items; load it as a reference instead"
        )));
    }
    let content = contents[0];
    let mime_type = content
        .get("mimeType")
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);

    if let Some(blob) = content.get("blob").and_then(Value::as_str) {
        let bytes = BASE64.decode(blob).map_err(|e| {
            SkillError::Io(format!("invalid base64 blob for MCP resource '{uri}': {e}"))
        })?;
        return Ok((bytes, mime_type));
    }
    if let Some(text) = content.get("text").and_then(Value::as_str) {
        return Ok((text.as_bytes().to_vec(), mime_type));
    }

    Err(SkillError::Io(format!(
        "MCP resource '{uri}' does not contain text or blob content"
    )))
}

#[async_trait]
impl Skill for McpPromptSkill {
    fn meta(&self) -> &SkillMeta {
        &self.meta
    }

    async fn read_instructions(&self) -> Result<String, SkillError> {
        #[derive(Serialize)]
        struct Frontmatter<'a> {
            name: &'a str,
            description: &'a str,
        }

        let body = format!(
            "This skill is backed by MCP prompt '{}' from server '{}'.\n\n{}\n{}\nUse the skill tool to activate it. When the prompt requires structured input, pass named arguments via the tool field 'arguments'. MCP resources from the same server can be loaded with load_skill_resource using paths like 'references/<resource-uri>' or 'assets/<resource-uri>'.\n",
            self.prompt_name,
            self.server_name,
            self.prompt_arguments_text(),
            self.resource_text(),
        );
        let frontmatter = serde_yaml::to_string(&Frontmatter {
            name: &self.meta.id,
            description: &self.meta.description,
        })
        .map_err(|e| SkillError::Io(e.to_string()))?;

        Ok(format!("---\n{frontmatter}---\n{body}"))
    }

    async fn activate(&self, args: Option<&Value>) -> Result<SkillActivation, SkillError> {
        let arguments = self.activation_arguments(args)?;
        let prompt = self
            .manager
            .get_prompt(&self.server_name, &self.prompt_name, arguments)
            .await
            .map_err(map_mcp_error)?;
        Ok(SkillActivation {
            instructions: self.render_prompt_result(prompt),
        })
    }

    async fn load_resource(
        &self,
        kind: SkillResourceKind,
        path: &str,
    ) -> Result<SkillResource, SkillError> {
        let uri = resource_uri_from_path(kind, path)?;
        let raw = self
            .manager
            .read_resource(&self.server_name, uri)
            .await
            .map_err(map_mcp_error)?;

        match kind {
            SkillResourceKind::Reference => {
                let text = read_text_resource(&raw, uri)?;
                materialize_reference_bytes(&self.meta.id, path, text.as_bytes())
                    .map(SkillResource::Reference)
                    .map_err(SkillError::from)
            }
            SkillResourceKind::Asset => {
                let (bytes, mime_type) = read_asset_resource(&raw, uri)?;
                materialize_asset_bytes(&self.meta.id, path, &bytes, mime_type)
                    .map(SkillResource::Asset)
                    .map_err(SkillError::from)
            }
        }
    }

    async fn run_script(&self, script: &str, _args: &[String]) -> Result<ScriptResult, SkillError> {
        Err(SkillError::Unsupported(format!(
            "MCP-backed skill '{}' does not support script execution: {script}",
            self.meta.id
        )))
    }
}

// ── Registry ──

#[derive(Clone, Default)]
struct McpPromptSkillRegistrySnapshot {
    version: u64,
    skills: HashMap<String, Arc<dyn Skill>>,
}

struct McpPromptSkillRegistryState {
    manager: Arc<McpToolRegistryManager>,
    snapshot: RwLock<McpPromptSkillRegistrySnapshot>,
    periodic_refresh: PeriodicRefresher,
}

async fn discover_snapshot_from_manager(
    manager: &Arc<McpToolRegistryManager>,
) -> Result<HashMap<String, Arc<dyn Skill>>, SkillRegistryManagerError> {
    let entries = match manager.list_prompts().await {
        Ok(entries) => entries,
        Err(err) if is_prompt_support_error(&err) => Vec::new(),
        Err(err) => return Err(SkillRegistryManagerError::Mcp(err.to_string())),
    };
    let resources_by_server =
        group_resources_by_server(manager.list_resources().await.unwrap_or_default());

    let mut skills = HashMap::new();
    for entry in entries {
        let resources = resources_by_server
            .get(&entry.server_name)
            .cloned()
            .unwrap_or_default();
        let skill = Arc::new(McpPromptSkill::from_entry(
            manager.clone(),
            entry,
            resources,
        )) as Arc<dyn Skill>;
        let id = skill.meta().id.trim().to_string();
        if id.is_empty() {
            return Err(SkillRegistryError::EmptySkillId.into());
        }
        if skills.insert(id.clone(), skill).is_some() {
            return Err(SkillRegistryError::DuplicateSkillId(id).into());
        }
    }
    Ok(skills)
}

fn group_resources_by_server(
    resources: Vec<McpResourceEntry>,
) -> HashMap<String, Vec<McpResourceHint>> {
    let mut grouped: HashMap<String, Vec<McpResourceHint>> = HashMap::new();
    for entry in resources {
        let server_name = entry.server_name.clone();
        grouped
            .entry(server_name)
            .or_default()
            .push(McpResourceHint::from_entry(entry));
    }
    for resource_hints in grouped.values_mut() {
        resource_hints.sort_by(|a, b| a.uri.cmp(&b.uri));
    }
    grouped
}

fn apply_snapshot(
    state: &McpPromptSkillRegistryState,
    skills: HashMap<String, Arc<dyn Skill>>,
) -> u64 {
    let mut snapshot = write_lock(&state.snapshot);
    let version = snapshot.version.saturating_add(1);
    *snapshot = McpPromptSkillRegistrySnapshot { version, skills };
    version
}

async fn refresh_state(
    state: &McpPromptSkillRegistryState,
) -> Result<u64, SkillRegistryManagerError> {
    let skills = discover_snapshot_from_manager(&state.manager).await?;
    Ok(apply_snapshot(state, skills))
}

/// Manages a registry of skills backed by MCP prompts.
///
/// Discovers MCP prompts from a [`McpToolRegistryManager`] and wraps each as a
/// [`Skill`] implementation. Supports periodic refresh to pick up new prompts.
#[derive(Clone)]
pub struct McpPromptSkillRegistryManager {
    state: Arc<McpPromptSkillRegistryState>,
}

impl std::fmt::Debug for McpPromptSkillRegistryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.state.snapshot);
        f.debug_struct("McpPromptSkillRegistryManager")
            .field("skills", &snapshot.skills.len())
            .field("version", &snapshot.version)
            .field(
                "periodic_refresh_running",
                &self.state.periodic_refresh.is_running(),
            )
            .finish()
    }
}

impl McpPromptSkillRegistryManager {
    /// Create a new registry by discovering prompts from the given MCP manager.
    pub async fn discover(
        manager: Arc<McpToolRegistryManager>,
    ) -> Result<Self, SkillRegistryManagerError> {
        let skills = discover_snapshot_from_manager(&manager).await?;
        Ok(Self {
            state: Arc::new(McpPromptSkillRegistryState {
                manager,
                snapshot: RwLock::new(McpPromptSkillRegistrySnapshot { version: 1, skills }),
                periodic_refresh: PeriodicRefresher::new(),
            }),
        })
    }

    /// Force a refresh of the prompt registry, returning the new version.
    pub async fn refresh(&self) -> Result<u64, SkillRegistryManagerError> {
        refresh_state(self.state.as_ref()).await
    }

    /// Current snapshot version.
    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }

    /// Start a background task that periodically refreshes the prompt registry.
    pub fn start_periodic_refresh(
        &self,
        interval: Duration,
    ) -> Result<(), SkillRegistryManagerError> {
        let weak_state = Arc::downgrade(&self.state);
        self.state
            .periodic_refresh
            .start(interval, move || {
                let weak = weak_state.clone();
                async move {
                    let Some(state) = weak.upgrade() else {
                        return;
                    };
                    if let Err(err) = refresh_state(state.as_ref()).await {
                        tracing::warn!(error = %err, "MCP prompt skill periodic refresh failed");
                    }
                }
            })
            .map_err(|msg| match msg.as_str() {
                m if m.contains("non-zero") => SkillRegistryManagerError::InvalidRefreshInterval,
                m if m.contains("already running") => {
                    SkillRegistryManagerError::PeriodicRefreshAlreadyRunning
                }
                _ => SkillRegistryManagerError::RuntimeUnavailable,
            })
    }

    /// Stop the periodic refresh task. Returns true if it was running.
    pub async fn stop_periodic_refresh(&self) -> bool {
        self.state.periodic_refresh.stop().await
    }

    /// Check whether the periodic refresh task is running.
    pub fn periodic_refresh_running(&self) -> bool {
        self.state.periodic_refresh.is_running()
    }
}

impl SkillRegistry for McpPromptSkillRegistryManager {
    fn len(&self) -> usize {
        read_lock(&self.state.snapshot).skills.len()
    }

    fn get(&self, id: &str) -> Option<Arc<dyn Skill>> {
        read_lock(&self.state.snapshot).skills.get(id).cloned()
    }

    fn ids(&self) -> Vec<String> {
        let snapshot = read_lock(&self.state.snapshot);
        let mut ids: Vec<String> = snapshot.skills.keys().cloned().collect();
        ids.sort();
        ids
    }

    fn snapshot(&self) -> HashMap<String, Arc<dyn Skill>> {
        read_lock(&self.state.snapshot).skills.clone()
    }

    fn start_periodic_refresh(&self, interval: Duration) -> Result<(), SkillRegistryManagerError> {
        McpPromptSkillRegistryManager::start_periodic_refresh(self, interval)
    }

    fn stop_periodic_refresh(&self) -> bool {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current()
                .block_on(McpPromptSkillRegistryManager::stop_periodic_refresh(self))
        })
    }

    fn periodic_refresh_running(&self) -> bool {
        McpPromptSkillRegistryManager::periodic_refresh_running(self)
    }
}

#[cfg(test)]
#[allow(dead_code, clippy::type_complexity)]
mod tests {
    use super::*;
    use awaken_ext_mcp::{
        McpProgressUpdate, McpPromptDefinition, McpResourceDefinition, McpToolTransport,
    };
    use mcp::transport::{McpServerConnectionConfig, ServerCapabilities, TransportTypeId};
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::mpsc;

    fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        match lock.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }

    #[derive(Debug)]
    struct MutablePromptTransport {
        prompts: RwLock<Vec<McpPromptDefinition>>,
        resource_defs: RwLock<Vec<McpResourceDefinition>>,
        resources: RwLock<HashMap<String, Value>>,
        prompt_calls: Mutex<Vec<(String, Option<HashMap<String, String>>)>>,
        resource_calls: Mutex<Vec<String>>,
        prompt_counter: AtomicUsize,
        prompt_list_calls: AtomicUsize,
        resource_list_calls: AtomicUsize,
        capabilities: Option<ServerCapabilities>,
    }

    impl MutablePromptTransport {
        fn new(prompts: Vec<McpPromptDefinition>) -> Self {
            Self {
                prompts: RwLock::new(prompts),
                resource_defs: RwLock::new(Vec::new()),
                resources: RwLock::new(HashMap::new()),
                prompt_calls: Mutex::new(Vec::new()),
                resource_calls: Mutex::new(Vec::new()),
                prompt_counter: AtomicUsize::new(0),
                prompt_list_calls: AtomicUsize::new(0),
                resource_list_calls: AtomicUsize::new(0),
                capabilities: None,
            }
        }

        fn with_capabilities(mut self, capabilities: ServerCapabilities) -> Self {
            self.capabilities = Some(capabilities);
            self
        }

        fn replace_prompts(&self, prompts: Vec<McpPromptDefinition>) {
            *write_lock(&self.prompts) = prompts;
        }

        fn with_resource(self, uri: &str, value: Value) -> Self {
            write_lock(&self.resources).insert(uri.to_string(), value);
            self
        }

        fn with_resource_definition(self, resource: McpResourceDefinition, value: Value) -> Self {
            write_lock(&self.resource_defs).push(resource.clone());
            write_lock(&self.resources).insert(resource.uri.clone(), value);
            self
        }

        fn prompt_calls(&self) -> Vec<(String, Option<HashMap<String, String>>)> {
            mutex_lock(&self.prompt_calls).clone()
        }

        fn prompt_list_calls(&self) -> usize {
            self.prompt_list_calls.load(Ordering::SeqCst)
        }

        fn resource_calls(&self) -> Vec<String> {
            mutex_lock(&self.resource_calls).clone()
        }

        fn resource_list_calls(&self) -> usize {
            self.resource_list_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl McpToolTransport for MutablePromptTransport {
        async fn list_tools(
            &self,
        ) -> Result<Vec<mcp::McpToolDefinition>, mcp::transport::McpTransportError> {
            Ok(Vec::new())
        }

        async fn list_prompts(
            &self,
        ) -> Result<Vec<McpPromptDefinition>, mcp::transport::McpTransportError> {
            self.prompt_list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(read_lock(&self.prompts).clone())
        }

        async fn list_resources(
            &self,
        ) -> Result<Vec<McpResourceDefinition>, mcp::transport::McpTransportError> {
            self.resource_list_calls.fetch_add(1, Ordering::SeqCst);
            Ok(read_lock(&self.resource_defs).clone())
        }

        async fn get_prompt(
            &self,
            name: &str,
            arguments: Option<HashMap<String, String>>,
        ) -> Result<McpPromptResult, mcp::transport::McpTransportError> {
            mutex_lock(&self.prompt_calls).push((name.to_string(), arguments.clone()));
            let seq = self.prompt_counter.fetch_add(1, Ordering::SeqCst) + 1;
            let mut ordered = BTreeMap::new();
            if let Some(arguments) = arguments {
                for (key, value) in arguments {
                    ordered.insert(key, value);
                }
            }
            Ok(McpPromptResult {
                description: Some(format!("prompt-{seq}")),
                messages: vec![awaken_ext_mcp::McpPromptMessage {
                    role: "user".to_string(),
                    content: json!({
                        "type": "text",
                        "text": format!("prompt={name};args={}", serde_json::to_string(&ordered).unwrap())
                    }),
                }],
            })
        }

        async fn call_tool(
            &self,
            _name: &str,
            _args: Value,
            _progress_tx: Option<mpsc::UnboundedSender<McpProgressUpdate>>,
        ) -> Result<mcp::CallToolResult, mcp::transport::McpTransportError> {
            Ok(mcp::CallToolResult {
                content: Vec::new(),
                structured_content: None,
                is_error: None,
            })
        }

        fn transport_type(&self) -> TransportTypeId {
            TransportTypeId::Stdio
        }

        async fn server_capabilities(
            &self,
        ) -> Result<Option<ServerCapabilities>, mcp::transport::McpTransportError> {
            Ok(self.capabilities.clone())
        }

        async fn read_resource(
            &self,
            uri: &str,
        ) -> Result<Value, mcp::transport::McpTransportError> {
            mutex_lock(&self.resource_calls).push(uri.to_string());
            read_lock(&self.resources).get(uri).cloned().ok_or_else(|| {
                mcp::transport::McpTransportError::TransportError(format!(
                    "unknown resource: {uri}"
                ))
            })
        }
    }

    fn prompt(name: &str, args: Vec<McpPromptArgument>) -> McpPromptDefinition {
        McpPromptDefinition {
            name: name.to_string(),
            title: Some(format!("{name} title")),
            description: Some(format!("{name} description")),
            arguments: args,
        }
    }

    fn prompt_arg(name: &str, required: bool) -> McpPromptArgument {
        McpPromptArgument {
            name: name.to_string(),
            description: Some(format!("{name} description")),
            required,
        }
    }

    async fn make_manager(transport: Arc<MutablePromptTransport>) -> Arc<McpToolRegistryManager> {
        Arc::new(
            McpToolRegistryManager::from_transports([(
                McpServerConnectionConfig::stdio("github", "node", vec!["server.js".to_string()]),
                transport as Arc<dyn McpToolTransport>,
            )])
            .await
            .unwrap(),
        )
    }

    #[tokio::test]
    async fn mcp_prompt_skill_activation_supports_named_arguments() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            vec![prompt_arg("path", true)],
        )]));
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager.clone())
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let activation = skill
            .activate(Some(&json!({"path": "src/lib.rs"})))
            .await
            .unwrap();

        assert_eq!(
            activation.instructions,
            "prompt=review;args={\"path\":\"src/lib.rs\"}"
        );
        let calls = transport.prompt_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "review");
        assert_eq!(
            calls[0].1.as_ref().and_then(|args| args.get("path")),
            Some(&"src/lib.rs".to_string())
        );
        assert_eq!(
            skill.meta().allowed_tools,
            vec!["mcp__github__*".to_string()]
        );
    }

    #[tokio::test]
    async fn mcp_prompt_skill_single_string_arg_maps_to_single_prompt_argument() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            vec![prompt_arg("path", true)],
        )]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let activation = skill.activate(Some(&json!("src/main.rs"))).await.unwrap();
        assert_eq!(
            activation.instructions,
            "prompt=review;args={\"path\":\"src/main.rs\"}"
        );
    }

    #[tokio::test]
    async fn mcp_prompt_skill_loads_text_resource_as_reference() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_resource(
                "file://guide.md",
                json!({
                    "contents": [{
                        "uri": "file://guide.md",
                        "text": "# Guide",
                        "mimeType": "text/markdown"
                    }]
                }),
            ),
        );
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let resource = skill
            .load_resource(SkillResourceKind::Reference, "references/file://guide.md")
            .await
            .unwrap();

        let SkillResource::Reference(reference) = resource else {
            panic!("expected reference resource");
        };
        assert_eq!(reference.skill, "mcp:github:review");
        assert_eq!(reference.path, "references/file://guide.md");
        assert_eq!(reference.content, "# Guide");
        assert_eq!(
            transport.resource_calls(),
            vec!["file://guide.md".to_string()]
        );
    }

    #[tokio::test]
    async fn mcp_prompt_skill_aggregates_multi_content_text_resource() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_resource(
                "file://guide.md",
                json!({
                    "contents": [
                        {
                            "uri": "file://other.md",
                            "text": "ignore me"
                        },
                        {
                            "uri": "file://guide.md",
                            "text": "# Guide"
                        },
                        {
                            "uri": "file://guide.md",
                            "text": "## Details"
                        }
                    ]
                }),
            ),
        );
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let resource = skill
            .load_resource(SkillResourceKind::Reference, "references/file://guide.md")
            .await
            .unwrap();

        let SkillResource::Reference(reference) = resource else {
            panic!("expected reference resource");
        };
        assert_eq!(reference.content, "# Guide\n\n## Details");
    }

    #[tokio::test]
    async fn mcp_prompt_skill_metadata_and_instructions_include_resource_hints() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", vec![prompt_arg("path", true)])])
                .with_resource_definition(
                    McpResourceDefinition {
                        uri: "file://guide.md".to_string(),
                        name: "guide".to_string(),
                        title: Some("Guide".to_string()),
                        description: Some("Guide".to_string()),
                        mime_type: Some("text/markdown".to_string()),
                        size: Some(1536),
                    },
                    json!({
                        "contents": [{
                            "uri": "file://guide.md",
                            "text": "# Guide"
                        }]
                    }),
                ),
        );
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        assert!(skill.meta().description.contains("Args: path (required)."));
        assert!(
            skill
                .meta()
                .description
                .contains("Resources: file://guide.md (Guide) - Guide [text/markdown; 1.5 KiB].")
        );

        let instructions = skill.read_instructions().await.unwrap();
        assert!(instructions.contains("Available MCP resources:"));
        assert!(instructions.contains("file://guide.md (Guide) - Guide [text/markdown; 1.5 KiB]"));
        assert_eq!(transport.resource_list_calls(), 1);
    }

    #[test]
    fn resource_hint_summary_includes_metadata_when_present() {
        let hint = McpResourceHint {
            uri: "file://logo.bin".to_string(),
            title: None,
            description: None,
            mime_type: Some("application/octet-stream".to_string()),
            size: Some(42),
        };

        assert_eq!(
            hint.summary(),
            "file://logo.bin [application/octet-stream; 42 B]"
        );
        assert_eq!(format_resource_size(1024), "1.0 KiB");
        assert_eq!(format_resource_size(1_048_576), "1.0 MiB");
    }

    #[tokio::test]
    async fn mcp_prompt_skill_loads_blob_resource_as_asset() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_resource(
                "file://logo.bin",
                json!({
                    "contents": [{
                        "uri": "file://logo.bin",
                        "blob": "aGVsbG8=",
                        "mimeType": "application/octet-stream"
                    }]
                }),
            ),
        );
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let resource = skill
            .load_resource(SkillResourceKind::Asset, "assets/file://logo.bin")
            .await
            .unwrap();

        let SkillResource::Asset(asset) = resource else {
            panic!("expected asset resource");
        };
        assert_eq!(asset.skill, "mcp:github:review");
        assert_eq!(asset.path, "assets/file://logo.bin");
        assert_eq!(
            asset.media_type.as_deref(),
            Some("application/octet-stream")
        );
        assert_eq!(asset.content, "aGVsbG8=");
    }

    #[tokio::test]
    async fn mcp_prompt_skill_rejects_multi_content_resource_loaded_as_asset() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_resource(
                "file://logo.bin",
                json!({
                    "contents": [
                        {
                            "uri": "file://logo.bin",
                            "blob": "aGVsbG8=",
                            "mimeType": "application/octet-stream"
                        },
                        {
                            "uri": "file://logo.bin",
                            "blob": "d29ybGQ=",
                            "mimeType": "application/octet-stream"
                        }
                    ]
                }),
            ),
        );
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Asset, "assets/file://logo.bin")
            .await
            .expect_err("multi-content asset should be rejected");
        assert!(
            matches!(err, SkillError::Unsupported(message) if message.contains("multiple content items"))
        );
    }

    #[tokio::test]
    async fn mcp_prompt_skill_rejects_binary_resource_loaded_as_reference() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_resource(
                "file://logo.bin",
                json!({
                    "contents": [{
                        "uri": "file://logo.bin",
                        "blob": "aGVsbG8="
                    }]
                }),
            ),
        );
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Reference, "references/file://logo.bin")
            .await
            .unwrap_err();

        assert!(matches!(err, SkillError::Unsupported(_)));
    }

    #[tokio::test]
    async fn mcp_prompt_skill_rejects_resource_contents_for_different_uri() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_resource(
                "file://guide.md",
                json!({
                    "contents": [{
                        "uri": "file://other.md",
                        "text": "wrong resource"
                    }]
                }),
            ),
        );
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Reference, "references/file://guide.md")
            .await
            .expect_err("mismatched uri should fail");

        assert!(matches!(err, SkillError::Io(message) if message.contains("different uri")));
    }

    #[tokio::test]
    async fn mcp_prompt_skill_rejects_ambiguous_string_args() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            vec![prompt_arg("path", true), prompt_arg("focus", false)],
        )]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill
            .activate(Some(&json!("src/main.rs")))
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn mcp_prompt_skill_registry_refresh_discovers_new_prompts() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        assert_eq!(registry.ids(), vec!["mcp:github:review".to_string()]);

        transport.replace_prompts(vec![
            prompt("review", Vec::new()),
            prompt("fix", Vec::new()),
        ]);
        let version = registry.refresh().await.unwrap();

        assert_eq!(version, 2);
        assert_eq!(
            registry.ids(),
            vec![
                "mcp:github:fix".to_string(),
                "mcp:github:review".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn mcp_prompt_skill_registry_periodic_refresh_updates_snapshot() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        registry
            .start_periodic_refresh(Duration::from_millis(20))
            .unwrap();

        transport.replace_prompts(vec![
            prompt("review", Vec::new()),
            prompt("fix", Vec::new()),
        ]);

        let start = std::time::Instant::now();
        while start.elapsed() < Duration::from_millis(400) {
            if registry.ids().iter().any(|id| id == "mcp:github:fix") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }

        assert!(registry.ids().iter().any(|id| id == "mcp:github:fix"));
        assert!(registry.stop_periodic_refresh().await);
    }

    #[test]
    fn sanitize_mcp_tool_component_replaces_special_chars() {
        assert_eq!(sanitize_mcp_tool_component("my-server"), "my_server");
        assert_eq!(sanitize_mcp_tool_component("a..b"), "a_b");
        assert_eq!(sanitize_mcp_tool_component("abc"), "abc");
        assert_eq!(sanitize_mcp_tool_component("--foo--"), "foo");
    }

    #[test]
    fn mcp_skill_id_format() {
        assert_eq!(mcp_skill_id("github", "review"), "mcp:github:review");
    }

    #[test]
    fn format_resource_size_ranges() {
        assert_eq!(format_resource_size(0), "0 B");
        assert_eq!(format_resource_size(512), "512 B");
        assert_eq!(format_resource_size(1024), "1.0 KiB");
        assert_eq!(format_resource_size(1_048_576), "1.0 MiB");
        assert_eq!(format_resource_size(1_073_741_824), "1.0 GiB");
    }

    // ── McpPromptSkill activation and instructions ──

    #[tokio::test]
    async fn mcp_prompt_skill_activation_with_no_args() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "simple",
            Vec::new(),
        )]));
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:simple").unwrap();

        let activation = skill.activate(None).await.unwrap();
        assert!(activation.instructions.contains("prompt=simple"));
        let calls = transport.prompt_calls();
        assert_eq!(calls.len(), 1);
        assert!(calls[0].1.is_none());
    }

    #[tokio::test]
    async fn mcp_prompt_skill_read_instructions_contains_frontmatter() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            vec![prompt_arg("path", true)],
        )]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let instructions = skill.read_instructions().await.unwrap();
        assert!(instructions.starts_with("---\n"));
        assert!(instructions.contains("name:"));
        assert!(instructions.contains("MCP prompt"));
        assert!(instructions.contains("path (required)"));
    }

    #[tokio::test]
    async fn mcp_prompt_skill_run_script_returns_unsupported() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill.run_script("scripts/run.sh", &[]).await.unwrap_err();
        assert!(matches!(err, SkillError::Unsupported(_)));
    }

    #[tokio::test]
    async fn mcp_prompt_skill_load_resource_bad_path_prefix() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Reference, "bad/path.md")
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn mcp_prompt_skill_load_resource_empty_uri() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();
        let skill = registry.get("mcp:github:review").unwrap();

        let err = skill
            .load_resource(SkillResourceKind::Reference, "references/")
            .await
            .unwrap_err();
        assert!(matches!(err, SkillError::InvalidArguments(_)));
    }

    // ── McpPromptSkillRegistryManager ──

    #[tokio::test]
    async fn mcp_prompt_skill_registry_list_returns_sorted_ids() {
        let transport = Arc::new(MutablePromptTransport::new(vec![
            prompt("beta", Vec::new()),
            prompt("alpha", Vec::new()),
        ]));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        let ids = registry.ids();
        assert_eq!(
            ids,
            vec![
                "mcp:github:alpha".to_string(),
                "mcp:github:beta".to_string()
            ]
        );
    }

    #[tokio::test]
    async fn mcp_prompt_skill_registry_version_increments_on_refresh() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        assert_eq!(registry.version(), 1);
        registry.refresh().await.unwrap();
        assert_eq!(registry.version(), 2);
    }

    #[tokio::test]
    async fn mcp_prompt_skill_registry_empty_when_no_prompts() {
        let transport = Arc::new(MutablePromptTransport::new(Vec::new()));
        let manager = make_manager(transport).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        assert!(registry.is_empty());
        assert_eq!(registry.ids(), Vec::<String>::new());
    }

    #[tokio::test]
    async fn mcp_prompt_skill_registry_concurrent_access_is_safe() {
        let transport = Arc::new(MutablePromptTransport::new(vec![prompt(
            "review",
            Vec::new(),
        )]));
        let manager = make_manager(transport.clone()).await;
        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        let reg1 = registry.clone();
        let reg2 = registry.clone();

        let (ids1, ids2) = tokio::join!(
            tokio::spawn(async move { reg1.ids() }),
            tokio::spawn(async move { reg2.ids() }),
        );

        assert_eq!(ids1.unwrap(), ids2.unwrap());
    }

    // ── Helper function tests ──

    #[test]
    fn enrich_description_with_args_and_resources() {
        let args = vec![prompt_arg("path", true), prompt_arg("focus", false)];
        let resources = vec![McpResourceHint {
            uri: "file://a.md".to_string(),
            title: Some("A".to_string()),
            description: None,
            mime_type: None,
            size: None,
        }];
        let desc = enrich_description("Base desc", &args, &resources);
        assert!(desc.contains("Args: path (required), focus."));
        assert!(desc.contains("Resources: file://a.md (A)."));
    }

    #[test]
    fn enrich_description_with_more_than_3_resources_shows_count() {
        let resources: Vec<McpResourceHint> = (0..5)
            .map(|i| McpResourceHint {
                uri: format!("file://{i}.md"),
                title: None,
                description: None,
                mime_type: None,
                size: None,
            })
            .collect();
        let desc = enrich_description("Base", &[], &resources);
        assert!(desc.contains("+2 more"));
    }

    #[test]
    fn is_prompt_support_error_detects_transport_message() {
        let err = McpError::Transport("list_prompts not supported".to_string());
        assert!(is_prompt_support_error(&err));

        let err = McpError::Transport("other error".to_string());
        assert!(!is_prompt_support_error(&err));

        let err = McpError::EmptyServerName;
        assert!(!is_prompt_support_error(&err));
    }

    #[test]
    fn map_mcp_error_maps_unsupported_capability() {
        let err = McpError::UnsupportedCapability {
            server_name: "s".to_string(),
            capability: "prompts",
        };
        let skill_err = map_mcp_error(err);
        assert!(matches!(skill_err, SkillError::Unsupported(_)));
    }

    #[test]
    fn map_mcp_error_maps_other_to_io() {
        let err = McpError::Transport("fail".to_string());
        let skill_err = map_mcp_error(err);
        assert!(matches!(skill_err, SkillError::Io(_)));
    }
}
