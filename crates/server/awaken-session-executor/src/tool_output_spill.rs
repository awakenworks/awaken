//! Session-sandbox materialization for oversized model-visible tool output.
//!
//! The threshold and preview format live here once. Native Runtime results and
//! external ACP result projections reach this adapter through the neutral
//! `ToolOutputSpiller` port; individual tools and transports stay unchanged.

use std::sync::Arc;

use async_trait::async_trait;
use awaken_agent_contract::agent::run::Id as RunId;
use awaken_runtime_contract::tool::{ToolError, ToolOutputSpiller};
use sha2::{Digest, Sha256};

use crate::session_environment::SessionEnvironment;

pub(crate) const MAX_INLINE_TOOL_OUTPUT_CHARS: usize = 100_000;
const OUTPUT_DIRECTORY: &str = ".awaken/tool-results";

pub(crate) struct SandboxToolOutputSpiller {
    environment: Arc<SessionEnvironment>,
}

impl SandboxToolOutputSpiller {
    pub(crate) fn new(environment: Arc<SessionEnvironment>) -> Self {
        Self { environment }
    }
}

#[async_trait]
impl ToolOutputSpiller for SandboxToolOutputSpiller {
    async fn spill(
        &self,
        run_id: &RunId,
        call_id: &str,
        content: String,
    ) -> Result<String, ToolError> {
        if content.chars().count() <= MAX_INLINE_TOOL_OUTPUT_CHARS {
            return Ok(content);
        }

        let logical_path = stable_output_path(run_id, call_id);
        self.environment
            .write_workspace_file(&logical_path, content.as_bytes())
            .await
            .map_err(|error| {
                ToolError::Execution(format!(
                    "write oversized tool output `{logical_path}`: {error}"
                ))
            })?;

        // Every backend starts tools/ACP in the Session workspace. A relative
        // path is therefore readable unchanged by Workdir's rooted tools and by
        // Namespace/Container `/workspace`; exposing a host absolute path would
        // both leak placement and be re-jailed incorrectly by Native path tools.
        Ok(truncated_preview(&content, &logical_path))
    }
}

fn stable_output_path(run_id: &RunId, call_id: &str) -> String {
    // A digest yields one bounded filename without trusting provider-authored call
    // ids as path components. The separator keeps `(ab,c)` distinct from `(a,bc)`.
    let mut digest = Sha256::new();
    digest.update(run_id.0.as_bytes());
    digest.update([0]);
    digest.update(call_id.as_bytes());
    let mut name = String::with_capacity(64);
    for byte in digest.finalize() {
        use std::fmt::Write as _;
        write!(name, "{byte:02x}").expect("writing digest into String");
    }
    format!("{OUTPUT_DIRECTORY}/{name}.txt")
}

fn truncated_preview(content: &str, visible_path: &str) -> String {
    let notice = format!(
        "\n\n[Tool output truncated: the complete output was written to {visible_path}. Read that file to access the full result.]"
    );
    let kept = MAX_INLINE_TOOL_OUTPUT_CHARS.saturating_sub(notice.chars().count());
    let mut preview: String = content.chars().take(kept).collect();
    preview.push_str(&notice);
    preview
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    use crate::session_environment::AgentSandbox;
    use awaken_provisioning_contract::{
        IsolationClass, NetworkPolicy, ResourceLimits, SandboxSpec,
    };
    use awaken_runtime_contract::llm::ToolCall;
    use awaken_sandbox_local::LocalProvider;

    fn spec() -> SandboxSpec {
        SandboxSpec {
            scope: "tool-output-spill".into(),
            isolation: IsolationClass::Workdir,
            mounts: Vec::new(),
            env: Vec::new(),
            packages: Default::default(),
            network: NetworkPolicy::Unrestricted,
            outputs_path: "/outputs".into(),
            requests: Default::default(),
            limits: ResourceLimits::default(),
            filesystem_continuity: awaken_provisioning_contract::FilesystemContinuity::Retained,
            lease_ttl_secs: None,
            environment: None,
            command: Vec::new(),
            deny_tool_egress: false,
        }
    }

    #[tokio::test]
    async fn character_boundary_spill_is_complete_bounded_and_idempotent() {
        // Cause-effect graph:
        // C1=character count <=100k; C2=count >100k; C3=UTF-8 multibyte input;
        // C4=same run/call is retried; C5=provider call id contains path syntax.
        // E1=content unchanged/no file; E2=complete bytes stored under the Session
        // jail; E3=preview+readable relative path is <=100k chars; E4=stable path
        // is overwritten idempotently; E5=provider text never becomes a path.
        //
        // | Rule | count | UTF-8 | retry/path syntax | Effects |
        // | S1 | <100k | any | no | E1 |
        // | S2 | =100k | yes | no | E1 |
        // | S3 | >100k | yes | no | E2,E3 |
        // | S4 | >100k | any | same key + `../` | E2,E3,E4,E5 |
        let base = tempfile::tempdir().unwrap();
        let local = LocalProvider::new(base.path())
            .create_sandbox(&spec())
            .await
            .unwrap();
        let environment = Arc::new(SessionEnvironment::workdir(local));
        let workspace = AgentSandbox::workspace_cwd(environment.as_ref());
        let spiller = SandboxToolOutputSpiller::new(environment.clone());
        let run_id = RunId("run/untrusted".into());
        let call_id = "../../provider-call";

        let below = "a".repeat(MAX_INLINE_TOOL_OUTPUT_CHARS - 1);
        assert_eq!(
            spiller
                .spill(&run_id, call_id, below.clone())
                .await
                .unwrap(),
            below,
            "S1"
        );
        let exact = "界".repeat(MAX_INLINE_TOOL_OUTPUT_CHARS);
        assert_eq!(
            spiller
                .spill(&run_id, call_id, exact.clone())
                .await
                .unwrap(),
            exact,
            "S2 counts characters rather than UTF-8 bytes"
        );
        assert!(
            !Path::new(&workspace).join(OUTPUT_DIRECTORY).exists(),
            "S1/S2 do not create spill storage"
        );

        let oversized = format!("{}界", "x".repeat(MAX_INLINE_TOOL_OUTPUT_CHARS));
        let preview = spiller
            .spill(&run_id, call_id, oversized.clone())
            .await
            .unwrap();
        let logical = stable_output_path(&run_id, call_id);
        let full_path = Path::new(&workspace).join(&logical);
        assert_eq!(
            std::fs::read_to_string(&full_path).unwrap(),
            oversized,
            "S3"
        );
        assert_eq!(preview.chars().count(), MAX_INLINE_TOOL_OUTPUT_CHARS, "S3");
        assert!(preview.contains(&logical), "S3");
        assert!(!logical.contains(".."), "S4/S5");
        let bash = environment
            .rooted_tools()
            .into_iter()
            .find(|tool| tool.id() == "bash")
            .expect("bash tool");
        let visible = bash
            .invoke(ToolCall {
                call_id: "verify-spill".into(),
                tool_id: "bash".into(),
                arguments: serde_json::json!({
                    "command": format!("/usr/bin/wc -c < {logical}")
                }),
            })
            .await
            .unwrap();
        assert_eq!(visible.text().trim(), oversized.len().to_string(), "S3");

        let replacement = "z".repeat(MAX_INLINE_TOOL_OUTPUT_CHARS + 1);
        let retried_preview = spiller
            .spill(&run_id, call_id, replacement.clone())
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&full_path).unwrap(),
            replacement,
            "S4"
        );
        assert!(retried_preview.contains(&logical), "S4");
    }
}
