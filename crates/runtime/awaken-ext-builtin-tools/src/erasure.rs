//! Compatibility path; the sole typed adapter is owned by the Runtime SDK.
pub use awaken_tool_contract::typed::{Erased, erase, erase_for};
#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use awaken_tool_contract::tool::{
        Tool, ToolConcurrency, ToolError, ToolResource, ToolResourceAccess,
    };
    use schemars::JsonSchema;
    use serde::Deserialize;

    use super::*;

    #[derive(Deserialize, JsonSchema)]
    struct WriteArgs {
        path: String,
    }

    struct TypedWrite;

    #[async_trait]
    impl Tool for TypedWrite {
        type Args = WriteArgs;
        type Output = String;

        const ID: &'static str = "typed_write";
        const DESCRIPTION: &'static str = "write one path";

        fn concurrency(&self, args: &Self::Args) -> ToolConcurrency {
            ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
                "workspace-path",
                &args.path,
            ))])
        }

        async fn call(&self, args: Self::Args) -> Result<Self::Output, ToolError> {
            Ok(args.path)
        }
    }

    #[test]
    fn typed_concurrency_classifier_uses_parsed_arguments_and_fails_closed() {
        // Boundary rule: valid wire JSON is classified from `WriteArgs`, giving
        // authors field access and compiler checks; malformed JSON never reaches
        // the classifier and remains Serial.
        let tool = erase(TypedWrite);
        assert_eq!(
            tool.concurrency(&serde_json::json!({"path": "src/lib.rs"})),
            ToolConcurrency::Resources(vec![ToolResourceAccess::Write(ToolResource::new(
                "workspace-path",
                "src/lib.rs"
            ))])
        );
        assert_eq!(
            tool.concurrency(&serde_json::json!({"path": 42})),
            ToolConcurrency::Serial
        );
    }
}
