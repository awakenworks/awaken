# Capability Matrix

This matrix maps each framework capability to the authoritative docs and concrete implementation paths in this repository.

| Capability | Primary docs | Example / implementation paths |
|---|---|---|
| Agent composition (`AgentDefinition`, behaviors, stop specs) | `reference/config.md`, `how-to/build-an-agent.md` | `crates/tirea-agentos/src/composition/agent_definition.rs`, `examples/src/starter_backend/mod.rs` |
| Stop policies and termination controls | `how-to/configure-stop-policies.md`, `reference/config.md`, `explanation/run-lifecycle-and-phases.md` | `crates/tirea-agentos/src/runtime/stop_policy_plugin.rs`, `examples/src/starter_backend/mod.rs`, `crates/tirea-agentos/src/runtime/tests.rs` |
| Tool execution modes | `reference/config.md`, `explanation/hitl-and-decision-flow.md` | `crates/tirea-agentos/src/composition/agent_definition.rs`, `examples/src/starter_backend/mod.rs` |
| Tool authoring and registration | `tutorials/first-tool.md`, `how-to/add-a-tool.md`, `reference/typed-tool.md` | `examples/src/starter_backend/tools.rs`, `examples/src/travel/tools.rs` |
| Plugin authoring and registration | `how-to/add-a-plugin.md`, `reference/derive-macro.md` | `crates/tirea-extension-reminder/src/lib.rs`, `crates/tirea-extension-permission/src/plugin.rs` |
| State patch operations + conflict model | `reference/state-ops.md`, `explanation/state-and-patch-model.md` | `crates/tirea-state/src/op.rs`, `crates/tirea-state/src/apply.rs` |
| Typed state derive (`#[derive(State)]`) | `reference/derive-macro.md` | `crates/tirea-state-derive/src/` |
| State scopes + run-scoped cleanup | `explanation/persistence-and-versioning.md`, `reference/config.md` | `crates/tirea-contract/src/lib.rs`, `crates/tirea-agentos/src/runtime/tests.rs`, `crates/tirea-agentos/src/runtime/stop_policy_plugin.rs` |
| HTTP SSE server surface | `reference/http-api.md`, `how-to/expose-http-sse.md` | `crates/tirea-agentos-server/src/http.rs`, `examples/src/starter_backend/mod.rs` |
| Canonical Run API (list/get/start/inputs/cancel) | `reference/run-api.md` | `crates/tirea-agentos-server/src/http.rs`, `crates/tirea-agentos-server/tests/run_api.rs` |
| Decision forwarding / suspend / replay | `explanation/hitl-and-decision-flow.md`, `reference/run-api.md`, `how-to/enable-tool-permission-hitl.md` | `crates/tirea-contract/src/io/decision.rs`, `crates/tirea-agent-loop/src/runtime/loop_runner/mod.rs`, `crates/tirea-agentos-server/tests/run_api.rs` |
| AG-UI protocol | `reference/protocols/ag-ui.md`, `how-to/integrate-copilotkit-ag-ui.md` | `crates/tirea-agentos-server/src/protocol/ag_ui/http.rs`, `examples/copilotkit-starter/lib/persisted-http-agent.ts`, `examples/travel-ui/lib/copilotkit-app.ts`, `examples/research-ui/lib/copilotkit-app.ts` |
| AI SDK v6 protocol | `reference/protocols/ai-sdk-v6.md`, `how-to/integrate-ai-sdk-frontend.md` | `crates/tirea-agentos-server/src/protocol/ai_sdk_v6/http.rs`, `examples/ai-sdk-starter/src/lib/transport.ts` |
| A2A protocol | `reference/protocols/a2a.md` | `crates/tirea-agentos-server/src/protocol/a2a/http.rs`, `crates/tirea-agentos-server/tests/a2a_http.rs` |
| NATS gateway transport | `reference/protocols/nats.md`, `how-to/expose-nats.md` | `crates/tirea-agentos-server/src/protocol/ag_ui/nats.rs`, `crates/tirea-agentos-server/src/protocol/ai_sdk_v6/nats.rs` |
| File thread/run storage | `how-to/use-file-store.md` | `crates/tirea-store-adapters/src/file_store.rs`, `crates/tirea-store-adapters/src/file_run_store.rs` |
| Postgres thread/run storage | `how-to/use-postgres-store.md` | `crates/tirea-store-adapters/src/postgres_store.rs` |
| NATS-buffered + Postgres durability | `how-to/use-nats-buffered-postgres-store.md` | `crates/tirea-store-adapters/src/nats_buffered.rs`, `crates/tirea-agentos-server/tests/e2e_nats_postgres.rs` |
| Tool permission + HITL approval | `how-to/enable-tool-permission-hitl.md`, `explanation/hitl-and-decision-flow.md` | `crates/tirea-extension-permission/src/`, `examples/src/starter_backend/mod.rs`, `examples/ai-sdk-starter/src/components/tools/permission-dialog.tsx`, `examples/travel-ui/hooks/useTripApproval.tsx`, `examples/research-ui/hooks/useDeleteApproval.tsx` |
| Reminder plugin | `how-to/use-reminder-plugin.md` | `crates/tirea-extension-reminder/src/` |
| LLM telemetry / observability | `how-to/enable-llmmetry-observability.md` | `crates/tirea-extension-observability/src/`, `examples/src/travel.rs` |
| Skills subsystem | `how-to/use-skills-subsystem.md` | `crates/tirea-extension-skills/src/subsystem.rs` |
| MCP tool bridge | `how-to/use-mcp-tools.md` | `crates/tirea-extension-mcp/src/lib.rs`, `examples/src/starter_backend/mod.rs` |
| Sub-agent delegation (`agent_run/stop/output`) | `how-to/use-sub-agent-delegation.md`, `explanation/sub-agent-delegation.md` | `crates/tirea-agentos/src/runtime/agent_tools/`, `crates/tirea-agentos/tests/real_multi_subagent_deepseek.rs` |
