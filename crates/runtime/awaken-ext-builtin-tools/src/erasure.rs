//! Erase a typed [`Tool`] into the dynamic [`RawTool`] boundary the runtime
//! registry invokes. Authors write typed tools (the preferred API); this single
//! adapter parses the call arguments into the tool's `Args`, runs it, and renders
//! the typed output into the model-visible result string. It is the bridge the
//! contract calls for (`awaken-runtime-contract`'s `tool` module) and lives in
//! the extension because a neutral crate may not implement `RawTool`.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_runtime_contract::tool::{
    RawTool, Tool, ToolCall, ToolError, ToolOutput, ToolRecoveryCapability,
};

/// Wraps a typed [`Tool`] and presents it as a schema-erased [`RawTool`].
pub struct Erased<T>(pub T);

/// Erase a typed tool into a registrable `RawTool` handle for
/// `Runtime::with_tool`.
pub fn erase<T: Tool + 'static>(tool: T) -> Arc<dyn RawTool> {
    Arc::new(Erased(tool))
}

#[async_trait]
impl<T: Tool> RawTool for Erased<T> {
    fn id(&self) -> &str {
        self.0.id()
    }

    fn recovery_capability(&self) -> ToolRecoveryCapability {
        self.0.recovery_capability()
    }

    async fn invoke(&self, call: ToolCall) -> Result<ToolOutput, ToolError> {
        // A null argument payload is an empty object, so a no-argument tool's
        // `Args` (e.g. an empty struct) still deserializes.
        let raw = if call.arguments.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            call.arguments
        };
        let args: T::Args = serde_json::from_value(raw)
            .map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
        let output = self.0.call(args).await?;
        Ok(ToolOutput::ok(call.call_id, render(&output)?))
    }
}

/// Render a typed output as the model-visible string: a bare string passes
/// through unquoted (file text and search output read naturally); anything
/// structured becomes compact JSON.
fn render<O: serde::Serialize>(output: &O) -> Result<String, ToolError> {
    match serde_json::to_value(output).map_err(|err| ToolError::Execution(err.to_string()))? {
        serde_json::Value::String(text) => Ok(text),
        other => serde_json::to_string(&other).map_err(|err| ToolError::Execution(err.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};

    struct Greet;
    #[derive(Deserialize)]
    struct GreetArgs {
        name: String,
    }
    #[async_trait]
    impl Tool for Greet {
        type Args = GreetArgs;
        type Output = String;
        fn id(&self) -> &str {
            "greet"
        }
        async fn call(&self, args: GreetArgs) -> Result<String, ToolError> {
            if args.name.is_empty() {
                return Err(ToolError::Execution("empty name".to_string()));
            }
            Ok(format!("hello, {}", args.name))
        }
    }

    struct Stats;
    #[derive(Deserialize)]
    struct StatsArgs {
        text: String,
    }
    #[derive(Serialize)]
    struct StatsOut {
        chars: usize,
    }
    #[async_trait]
    impl Tool for Stats {
        type Args = StatsArgs;
        type Output = StatsOut;
        fn id(&self) -> &str {
            "stats"
        }
        async fn call(&self, args: StatsArgs) -> Result<StatsOut, ToolError> {
            Ok(StatsOut {
                chars: args.text.chars().count(),
            })
        }
    }

    fn call(tool_id: &str, args: serde_json::Value) -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            tool_id: tool_id.to_string(),
            arguments: args,
        }
    }

    #[tokio::test]
    async fn erased_tool_keeps_the_typed_id() {
        assert_eq!(RawTool::id(&Erased(Greet)), "greet");
        assert_eq!(RawTool::id(&Erased(Stats)), "stats");
    }

    #[tokio::test]
    async fn string_output_renders_unquoted() {
        let raw = erase(Greet);
        let out = raw
            .invoke(call("greet", serde_json::json!({ "name": "Ada" })))
            .await
            .expect("invoke");
        assert_eq!(out.content, "hello, Ada");
        assert!(!out.is_error);
        assert_eq!(out.call_id, "c1");
    }

    #[tokio::test]
    async fn structured_output_renders_as_json() {
        let raw = erase(Stats);
        let out = raw
            .invoke(call("stats", serde_json::json!({ "text": "héllo" })))
            .await
            .expect("invoke");
        assert_eq!(out.content, r#"{"chars":5}"#);
    }

    #[tokio::test]
    async fn invalid_arguments_are_a_typed_error() {
        let raw = erase(Greet);
        let err = raw
            .invoke(call("greet", serde_json::json!({ "wrong": 1 })))
            .await
            .expect_err("missing name");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn null_args_for_a_required_field_tool_is_invalid_arguments() {
        // The null→empty-object coalescing must not fabricate a default for a
        // required field: an empty object still fails deserialization as a typed
        // InvalidArguments error (only genuinely no-arg tools survive a null call).
        let raw = erase(Greet);
        let err = raw
            .invoke(call("greet", serde_json::Value::Null))
            .await
            .expect_err("null args cannot satisfy a required `name`");
        assert!(matches!(err, ToolError::InvalidArguments(_)));
    }

    #[tokio::test]
    async fn tool_execution_error_propagates() {
        let raw = erase(Greet);
        let err = raw
            .invoke(call("greet", serde_json::json!({ "name": "" })))
            .await
            .expect_err("empty name");
        assert!(matches!(err, ToolError::Execution(_)));
    }
}
