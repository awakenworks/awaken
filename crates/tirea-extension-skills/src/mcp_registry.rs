use crate::{
    ScriptResult, Skill, SkillActivation, SkillError, SkillMeta, SkillRegistry, SkillRegistryError,
    SkillRegistryManagerError, SkillResource, SkillResourceKind,
};
use async_trait::async_trait;
use serde::Serialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, RwLock, Weak};
use std::time::Duration;
use tirea_extension_mcp::{
    McpPromptArgument, McpPromptEntry, McpPromptResult, McpToolRegistryError,
    McpToolRegistryManager,
};
use tokio::runtime::Handle;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

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

fn mutex_lock<T>(lock: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    match lock.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn is_prompt_support_error(err: &McpToolRegistryError) -> bool {
    matches!(err, McpToolRegistryError::Transport(msg) if msg.contains("list_prompts not supported"))
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
pub struct McpPromptSkill {
    meta: SkillMeta,
    server_name: String,
    prompt_name: String,
    prompt_arguments: Vec<McpPromptArgument>,
    manager: Arc<McpToolRegistryManager>,
}

impl McpPromptSkill {
    pub fn from_entry(manager: Arc<McpToolRegistryManager>, entry: McpPromptEntry) -> Self {
        let prompt = entry.prompt;
        let skill_id = mcp_skill_id(&entry.server_name, &prompt.name);
        let display_name = prompt.title.clone().unwrap_or_else(|| prompt.name.clone());
        let description = prompt.description.clone().unwrap_or_else(|| {
            format!(
                "MCP prompt '{}' from server '{}'",
                prompt.name, entry.server_name
            )
        });
        let server_tool_pattern = format!(
            "mcp__{}__*",
            sanitize_mcp_tool_component(&entry.server_name)
        );

        Self {
            meta: SkillMeta {
                id: skill_id,
                name: display_name,
                description,
                allowed_tools: vec![server_tool_pattern],
            },
            server_name: entry.server_name,
            prompt_name: prompt.name,
            prompt_arguments: prompt.arguments,
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
        if prompt.messages.len() == 1 && prompt.messages[0].role.eq_ignore_ascii_case("user") {
            if let Some(text) = prompt.messages[0]
                .content
                .get("text")
                .and_then(Value::as_str)
            {
                return text.to_string();
            }
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
            "This skill is backed by MCP prompt '{}' from server '{}'.\n\n{}\nUse the skill tool to activate it. When the prompt requires structured input, pass named arguments via the tool field 'arguments'.\n",
            self.prompt_name,
            self.server_name,
            self.prompt_arguments_text(),
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
            .map_err(|e| SkillError::Io(e.to_string()))?;
        Ok(SkillActivation {
            instructions: self.render_prompt_result(prompt),
        })
    }

    async fn load_resource(
        &self,
        _kind: SkillResourceKind,
        _path: &str,
    ) -> Result<SkillResource, SkillError> {
        Err(SkillError::Unsupported(format!(
            "MCP-backed skill '{}' does not expose local skill resources",
            self.meta.id
        )))
    }

    async fn run_script(&self, script: &str, _args: &[String]) -> Result<ScriptResult, SkillError> {
        Err(SkillError::Unsupported(format!(
            "MCP-backed skill '{}' does not support script execution: {script}",
            self.meta.id
        )))
    }
}

#[derive(Clone, Default)]
struct McpPromptSkillRegistrySnapshot {
    version: u64,
    skills: HashMap<String, Arc<dyn Skill>>,
}

struct PeriodicRefreshRuntime {
    stop_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<()>,
}

struct McpPromptSkillRegistryState {
    manager: Arc<McpToolRegistryManager>,
    snapshot: RwLock<McpPromptSkillRegistrySnapshot>,
    periodic_refresh: Mutex<Option<PeriodicRefreshRuntime>>,
}

async fn discover_snapshot_from_manager(
    manager: &Arc<McpToolRegistryManager>,
) -> Result<HashMap<String, Arc<dyn Skill>>, SkillRegistryManagerError> {
    let entries = match manager.list_prompts().await {
        Ok(entries) => entries,
        Err(err) if is_prompt_support_error(&err) => Vec::new(),
        Err(err) => return Err(err.into()),
    };

    let mut skills = HashMap::new();
    for entry in entries {
        let skill = Arc::new(McpPromptSkill::from_entry(manager.clone(), entry)) as Arc<dyn Skill>;
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

async fn periodic_refresh_loop(
    state: Weak<McpPromptSkillRegistryState>,
    interval: Duration,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut ticker = tokio::time::interval(interval);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    ticker.tick().await;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,
            _ = ticker.tick() => {
                let Some(state) = state.upgrade() else {
                    break;
                };
                if let Err(err) = refresh_state(state.as_ref()).await {
                    tracing::warn!(error = %err, "MCP prompt skill periodic refresh failed");
                }
            }
        }
    }
}

#[derive(Clone)]
pub struct McpPromptSkillRegistryManager {
    state: Arc<McpPromptSkillRegistryState>,
}

impl std::fmt::Debug for McpPromptSkillRegistryManager {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let snapshot = read_lock(&self.state.snapshot);
        let periodic_running = self.periodic_refresh_running();
        f.debug_struct("McpPromptSkillRegistryManager")
            .field("skills", &snapshot.skills.len())
            .field("version", &snapshot.version)
            .field("periodic_refresh_running", &periodic_running)
            .finish()
    }
}

impl McpPromptSkillRegistryManager {
    pub async fn discover(
        manager: Arc<McpToolRegistryManager>,
    ) -> Result<Self, SkillRegistryManagerError> {
        let skills = discover_snapshot_from_manager(&manager).await?;
        Ok(Self {
            state: Arc::new(McpPromptSkillRegistryState {
                manager,
                snapshot: RwLock::new(McpPromptSkillRegistrySnapshot { version: 1, skills }),
                periodic_refresh: Mutex::new(None),
            }),
        })
    }

    pub async fn refresh(&self) -> Result<u64, SkillRegistryManagerError> {
        refresh_state(self.state.as_ref()).await
    }

    pub fn version(&self) -> u64 {
        read_lock(&self.state.snapshot).version
    }

    pub fn start_periodic_refresh(
        &self,
        interval: Duration,
    ) -> Result<(), SkillRegistryManagerError> {
        if interval.is_zero() {
            return Err(SkillRegistryManagerError::InvalidRefreshInterval);
        }

        let handle =
            Handle::try_current().map_err(|_| SkillRegistryManagerError::RuntimeUnavailable)?;
        let mut runtime = mutex_lock(&self.state.periodic_refresh);
        if runtime
            .as_ref()
            .is_some_and(|running| !running.join.is_finished())
        {
            return Err(SkillRegistryManagerError::PeriodicRefreshAlreadyRunning);
        }

        let (stop_tx, stop_rx) = oneshot::channel();
        let weak_state = Arc::downgrade(&self.state);
        let join = handle.spawn(periodic_refresh_loop(weak_state, interval, stop_rx));
        *runtime = Some(PeriodicRefreshRuntime {
            stop_tx: Some(stop_tx),
            join,
        });
        Ok(())
    }

    pub fn stop_periodic_refresh(&self) -> bool {
        let runtime = {
            let mut guard = mutex_lock(&self.state.periodic_refresh);
            guard.take()
        };

        let Some(mut runtime) = runtime else {
            return false;
        };

        if let Some(stop_tx) = runtime.stop_tx.take() {
            let _ = stop_tx.send(());
        }
        runtime.join.abort();
        true
    }

    pub fn periodic_refresh_running(&self) -> bool {
        let mut runtime = mutex_lock(&self.state.periodic_refresh);
        if runtime
            .as_ref()
            .is_some_and(|running| running.join.is_finished())
        {
            *runtime = None;
            return false;
        }
        runtime.is_some()
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
        McpPromptSkillRegistryManager::stop_periodic_refresh(self)
    }

    fn periodic_refresh_running(&self) -> bool {
        McpPromptSkillRegistryManager::periodic_refresh_running(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tirea_extension_mcp::{McpProgressUpdate, McpPromptDefinition, McpToolTransport};
    use tokio::sync::mpsc;

    #[derive(Debug)]
    struct MutablePromptTransport {
        prompts: RwLock<Vec<McpPromptDefinition>>,
        prompt_calls: Mutex<Vec<(String, Option<HashMap<String, String>>)>>,
        prompt_counter: AtomicUsize,
        prompt_list_calls: AtomicUsize,
        capabilities: Option<mcp::transport::ServerCapabilities>,
    }

    impl MutablePromptTransport {
        fn new(prompts: Vec<McpPromptDefinition>) -> Self {
            Self {
                prompts: RwLock::new(prompts),
                prompt_calls: Mutex::new(Vec::new()),
                prompt_counter: AtomicUsize::new(0),
                prompt_list_calls: AtomicUsize::new(0),
                capabilities: None,
            }
        }

        fn with_capabilities(mut self, capabilities: mcp::transport::ServerCapabilities) -> Self {
            self.capabilities = Some(capabilities);
            self
        }

        fn replace_prompts(&self, prompts: Vec<McpPromptDefinition>) {
            *write_lock(&self.prompts) = prompts;
        }

        fn prompt_calls(&self) -> Vec<(String, Option<HashMap<String, String>>)> {
            mutex_lock(&self.prompt_calls).clone()
        }

        fn prompt_list_calls(&self) -> usize {
            self.prompt_list_calls.load(Ordering::SeqCst)
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
                messages: vec![tirea_extension_mcp::McpPromptMessage {
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

        fn transport_type(&self) -> mcp::transport::TransportTypeId {
            mcp::transport::TransportTypeId::Stdio
        }

        async fn server_capabilities(
            &self,
        ) -> Result<Option<mcp::transport::ServerCapabilities>, mcp::transport::McpTransportError>
        {
            Ok(self.capabilities.clone())
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
                mcp::transport::McpServerConnectionConfig::stdio(
                    "github",
                    "node",
                    vec!["server.js".to_string()],
                ),
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
    async fn mcp_prompt_skill_registry_skips_servers_without_prompt_capability() {
        let transport = Arc::new(
            MutablePromptTransport::new(vec![prompt("review", Vec::new())]).with_capabilities(
                mcp::transport::ServerCapabilities {
                    prompts: None,
                    ..mcp::transport::ServerCapabilities::default()
                },
            ),
        );
        let manager = make_manager(transport.clone()).await;

        let registry = McpPromptSkillRegistryManager::discover(manager)
            .await
            .unwrap();

        assert!(registry.ids().is_empty());
        assert_eq!(transport.prompt_list_calls(), 0);
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
        assert!(registry.stop_periodic_refresh());
    }
}
