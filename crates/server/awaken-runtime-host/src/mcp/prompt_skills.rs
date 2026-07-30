//! MCP prompt-to-instruction-only-skill adapter.
//!
//! Discovery snapshots prompt metadata (`prompts/list`). Activation keeps the
//! remote semantics intact and resolves the body lazily with `prompts/get`.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use async_trait::async_trait;
use awaken_ext_mcp::transport::McpToolTransport;
use awaken_ext_mcp::{McpPromptArgument, McpPromptDefinition, McpPromptResult};
use serde_json::Value;

use awaken_ext_skills::{SkillProvenance, SkillRegistry, SkillSpec};

pub struct McpPromptSkillRegistry {
    server_name: String,
    transport: Arc<dyn McpToolTransport>,
    prompts: BTreeMap<String, McpPromptDefinition>,
}

impl McpPromptSkillRegistry {
    pub async fn discover(
        server_name: impl Into<String>,
        transport: Arc<dyn McpToolTransport>,
    ) -> Result<Self, String> {
        let server_name = server_name.into();
        let prompts = transport
            .list_prompts()
            .await
            .map_err(|error| format!("mcp server `{server_name}` prompts/list: {error}"))?
            .into_iter()
            .map(|prompt| (skill_id(&server_name, &prompt.name), prompt))
            .collect();
        Ok(Self {
            server_name,
            transport,
            prompts,
        })
    }

    fn spec(&self, id: &str, prompt: &McpPromptDefinition) -> SkillSpec {
        let mut spec = SkillSpec::new(
            id,
            prompt.title.as_deref().unwrap_or(&prompt.name),
            prompt.description.clone().unwrap_or_else(|| {
                format!(
                    "MCP prompt `{}` from server `{}`",
                    prompt.name, self.server_name
                )
            }),
            "",
        )
        .with_provenance(SkillProvenance::Mcp);
        // Slash expansion is synchronous and only safe for already-materialized
        // file skills. Remote prompts must pass through the async Skill tool so
        // required arguments and prompts/get failures remain explicit.
        spec.user_invocable = false;
        spec.arguments = prompt
            .arguments
            .iter()
            .map(|arg| arg.name.clone())
            .collect();
        if !prompt.arguments.is_empty() {
            spec.argument_hint = Some(argument_hint(&prompt.arguments));
        }
        spec
    }
}

#[async_trait]
impl SkillRegistry for McpPromptSkillRegistry {
    fn get(&self, id: &str) -> Option<SkillSpec> {
        self.prompts.get(id).map(|prompt| self.spec(id, prompt))
    }

    fn list(&self) -> Vec<SkillSpec> {
        self.prompts
            .iter()
            .map(|(id, prompt)| self.spec(id, prompt))
            .collect()
    }

    async fn resolve(
        &self,
        id: &str,
        arguments: Option<Value>,
    ) -> Result<Option<SkillSpec>, String> {
        let Some(prompt) = self.prompts.get(id) else {
            return Ok(None);
        };
        let arguments = activation_arguments(id, &prompt.arguments, arguments)?;
        let result = self
            .transport
            .get_prompt(&prompt.name, arguments)
            .await
            .map_err(|error| {
                format!(
                    "mcp skill `{id}` prompts/get from `{}` failed: {error}",
                    self.server_name
                )
            })?;
        let mut spec = self.spec(id, prompt);
        spec.body = render_prompt_result(&self.server_name, &prompt.name, result);
        Ok(Some(spec))
    }
}

fn skill_id(server_name: &str, prompt_name: &str) -> String {
    format!("mcp:{server_name}:{prompt_name}")
}

fn argument_hint(arguments: &[McpPromptArgument]) -> String {
    arguments
        .iter()
        .map(|argument| {
            if argument.required {
                format!("{} (required)", argument.name)
            } else {
                format!("{} (optional)", argument.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn activation_arguments(
    skill_id: &str,
    definitions: &[McpPromptArgument],
    arguments: Option<Value>,
) -> Result<Option<HashMap<String, String>>, String> {
    let Some(arguments) = arguments else {
        if let Some(required) = definitions.iter().find(|argument| argument.required) {
            return Err(format!(
                "mcp skill `{skill_id}` requires named argument `{}`",
                required.name
            ));
        }
        return Ok(None);
    };
    let mut resolved = HashMap::new();
    match arguments {
        Value::String(value) if definitions.len() == 1 => {
            resolved.insert(definitions[0].name.clone(), value);
        }
        Value::Object(values) => {
            for (name, value) in values {
                if !definitions.iter().any(|argument| argument.name == name) {
                    return Err(format!(
                        "mcp skill `{skill_id}` does not declare argument `{name}`"
                    ));
                }
                let value = match value {
                    Value::String(value) => value,
                    Value::Bool(value) => value.to_string(),
                    Value::Number(value) => value.to_string(),
                    _ => {
                        return Err(format!(
                            "mcp skill `{skill_id}` argument `{name}` must be scalar"
                        ));
                    }
                };
                resolved.insert(name, value);
            }
        }
        Value::Null => {}
        _ => {
            return Err(format!("mcp skill `{skill_id}` requires named arguments"));
        }
    }
    if let Some(required) = definitions
        .iter()
        .find(|argument| argument.required && !resolved.contains_key(&argument.name))
    {
        return Err(format!(
            "mcp skill `{skill_id}` requires named argument `{}`",
            required.name
        ));
    }
    Ok((!resolved.is_empty()).then_some(resolved))
}

fn render_prompt_result(server_name: &str, prompt_name: &str, result: McpPromptResult) -> String {
    if result.messages.len() == 1
        && result.messages[0].role.eq_ignore_ascii_case("user")
        && let Some(text) = result.messages[0]
            .content
            .get("text")
            .and_then(Value::as_str)
    {
        return text.to_string();
    }
    let mut output =
        format!("<mcp_skill_prompt server=\"{server_name}\" prompt=\"{prompt_name}\">\n");
    for message in result.messages {
        output.push_str(&format!("<message role=\"{}\">\n", message.role));
        if let Some(text) = message
            .content
            .as_str()
            .or_else(|| message.content.get("text").and_then(Value::as_str))
        {
            output.push_str(text);
        } else {
            output.push_str(&message.content.to_string());
        }
        output.push_str("\n</message>\n");
    }
    output.push_str("</mcp_skill_prompt>");
    output
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use awaken_ext_mcp::{CallToolResult, McpPromptMessage, McpToolDefinition, McpTransportError};
    use awaken_runtime_contract::tool::{RawTool, ToolCall};

    use super::*;

    type RecordedPromptRequest = (String, Option<HashMap<String, String>>);

    struct PromptTransport {
        seen: Mutex<Vec<RecordedPromptRequest>>,
        fail_get: bool,
    }

    #[async_trait]
    impl McpToolTransport for PromptTransport {
        async fn list_tools(&self) -> Result<Vec<McpToolDefinition>, McpTransportError> {
            Ok(Vec::new())
        }

        async fn call_tool(
            &self,
            _tool_name: &str,
            _arguments: Value,
        ) -> Result<CallToolResult, McpTransportError> {
            unreachable!("instruction-only skills do not call MCP tools during activation")
        }

        async fn list_prompts(&self) -> Result<Vec<McpPromptDefinition>, McpTransportError> {
            Ok(vec![McpPromptDefinition {
                name: "review".into(),
                title: Some("Review".into()),
                description: Some("Review a change".into()),
                arguments: vec![McpPromptArgument {
                    name: "focus".into(),
                    description: Some("review focus".into()),
                    required: true,
                }],
            }])
        }

        async fn get_prompt(
            &self,
            name: &str,
            arguments: Option<HashMap<String, String>>,
        ) -> Result<McpPromptResult, McpTransportError> {
            self.seen
                .lock()
                .unwrap()
                .push((name.to_string(), arguments.clone()));
            if self.fail_get {
                return Err(McpTransportError::TransportError(
                    "remote unavailable".into(),
                ));
            }
            Ok(McpPromptResult {
                description: None,
                messages: vec![McpPromptMessage {
                    role: "user".into(),
                    content: serde_json::json!({
                        "type": "text",
                        "text": format!(
                            "Review with focus {}",
                            arguments
                                .as_ref()
                                .and_then(|values| values.get("focus"))
                                .cloned()
                                .unwrap_or_default()
                        )
                    }),
                }],
            })
        }
    }

    #[tokio::test]
    async fn prompt_is_one_lazy_instruction_only_skill_with_named_arguments() {
        let transport = Arc::new(PromptTransport {
            seen: Mutex::new(Vec::new()),
            fail_get: false,
        });
        let registry = McpPromptSkillRegistry::discover("github", transport.clone())
            .await
            .expect("discovers prompt metadata");

        let listed = registry.list();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "mcp:github:review");
        assert_eq!(listed[0].provenance, SkillProvenance::Mcp);
        assert!(!listed[0].user_invocable);
        assert!(
            listed[0].body.is_empty(),
            "discovery must not fetch the body"
        );
        assert!(transport.seen.lock().unwrap().is_empty());

        let resolved = registry
            .resolve(
                "mcp:github:review",
                Some(serde_json::json!({ "focus": "security" })),
            )
            .await
            .expect("activation succeeds")
            .expect("skill exists");
        assert_eq!(resolved.body, "Review with focus security");
        assert_eq!(
            transport.seen.lock().unwrap().as_slice(),
            &[(
                "review".to_string(),
                Some(HashMap::from([(
                    "focus".to_string(),
                    "security".to_string()
                )]))
            )]
        );
    }

    #[tokio::test]
    async fn missing_required_prompt_argument_fails_before_remote_get() {
        let transport = Arc::new(PromptTransport {
            seen: Mutex::new(Vec::new()),
            fail_get: false,
        });
        let registry = McpPromptSkillRegistry::discover("github", transport.clone())
            .await
            .unwrap();
        let error = registry
            .resolve("mcp:github:review", None)
            .await
            .unwrap_err();
        assert!(error.contains("requires named argument `focus`"), "{error}");
        assert!(transport.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn invalid_named_prompt_arguments_fail_before_remote_get() {
        let transport = Arc::new(PromptTransport {
            seen: Mutex::new(Vec::new()),
            fail_get: false,
        });
        let registry = McpPromptSkillRegistry::discover("github", transport.clone())
            .await
            .unwrap();
        for arguments in [
            serde_json::json!({ "unknown": "value" }),
            serde_json::json!({ "focus": ["security"] }),
        ] {
            assert!(
                registry
                    .resolve("mcp:github:review", Some(arguments))
                    .await
                    .is_err()
            );
        }
        assert!(transport.seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn remote_prompt_get_failure_is_not_converted_to_an_empty_skill() {
        let transport = Arc::new(PromptTransport {
            seen: Mutex::new(Vec::new()),
            fail_get: true,
        });
        let registry = McpPromptSkillRegistry::discover("github", transport)
            .await
            .unwrap();
        let error = registry
            .resolve(
                "mcp:github:review",
                Some(serde_json::json!({ "focus": "security" })),
            )
            .await
            .unwrap_err();
        assert!(error.contains("remote unavailable"), "{error}");
    }

    #[tokio::test]
    async fn ordinary_skill_tool_activates_an_mcp_prompt_without_a_separate_surface() {
        let transport = Arc::new(PromptTransport {
            seen: Mutex::new(Vec::new()),
            fail_get: false,
        });
        let registry: Arc<dyn SkillRegistry> = Arc::new(
            McpPromptSkillRegistry::discover("github", transport)
                .await
                .unwrap(),
        );
        let tool = awaken_ext_skills::SkillTool::new(registry);
        let output = tool
            .invoke(ToolCall {
                call_id: "activate-1".into(),
                tool_id: awaken_ext_skills::SKILL_TOOL_ID.into(),
                arguments: serde_json::json!({
                    "skill": "mcp:github:review",
                    "arguments": { "focus": "correctness" }
                }),
            })
            .await
            .expect("tool invocation completes");
        assert!(!output.is_error, "{}", output.content);
        assert!(output.content.contains("Review with focus correctness"));
    }
}
