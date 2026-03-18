# Multi-Agent Design Patterns

## Natural-language orchestration

Tirea uses **natural-language orchestration**, inspired by Claude Code's sub-agent model. The LLM decides when to delegate, to whom, and how to combine results. You define each agent's identity and access policy; the runtime handles everything else. There are no DAGs, no state machines, and no explicit routing code — unlike frameworks such as LangGraph or Google ADK where you wire agents into graphs and define transitions in code.

This works because the runtime provides:
- **Agent registry** — agents registered at build time are rendered into the system prompt, so the LLM always knows who it can delegate to
- **Background execution with completion notifications** — sub-agents run in the background; the runtime injects their status after each tool call, keeping the LLM aware of what's running, finished, or failed
- **Foreground and background modes** — block until a sub-agent finishes, or run multiple concurrently and receive completion notifications
- **Thread isolation** — each sub-agent runs in its own thread with independent state
- **Orphan recovery** — orphaned sub-agents are detected and resumed on restart

## Patterns

All patterns below are implemented with the same building blocks:

- `agent_run` / `agent_stop` / `agent_output` delegation tools
- `agent_handoff` for same-thread agent switching
- `AgentDefinition` with `allowed_agents` / `excluded_agents` / `allowed_tools`
- `SuspendTicket` for human-in-the-loop gating
- System prompt engineering for control flow

No dedicated workflow agent types are needed. The LLM-driven loop plus delegation tools cover these patterns through prompt configuration.

## Pattern Index

| Pattern | Tirea Mechanism | Deterministic Flow? |
|---|---|---|
| [Coordinator](#coordinator) | `agent_run` + prompt routing | No (LLM decides) |
| [Sequential Pipeline](#sequential-pipeline) | Chained `agent_run` calls | No (LLM relays) |
| [Parallel Fan-Out/Gather](#parallel-fan-outgather) | Parallel `agent_run` + `agent_output` | No (best-effort) |
| [Hierarchical Decomposition](#hierarchical-decomposition) | Nested `agent_run` | No |
| [Generator-Critic](#generator-critic) | Generator as main agent, critic as child via `agent_run` | No |
| [Iterative Refinement](#iterative-refinement) | Same as Generator-Critic with additional refiner child | No |
| [Human-in-the-Loop](#human-in-the-loop) | `SuspendTicket` + `PermissionPlugin` | Yes (runtime gating) |
| [Swarm / Peer Handoff](#swarm--peer-handoff) | `agent_handoff` + `HandoffPlugin` overlay | No (LLM decides) |

## Coordinator

A single orchestrator agent analyzes user intent and routes to specialized worker agents.

```text
User -> [Orchestrator] -> intent analysis
                       -> agent_run("billing")   if billing question
                       -> agent_run("support")   if technical issue
                       -> agent_run("sales")     if pricing question
```

### Setup

```rust,ignore
let os = AgentOs::builder()
    .with_agent_spec(AgentDefinitionSpec::local_with_id("billing", AgentDefinition::new("deepseek-chat")
        .with_system_prompt("You are a billing specialist.")
        .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()])))
    .with_agent_spec(AgentDefinitionSpec::local_with_id("support", AgentDefinition::new("deepseek-chat")
        .with_system_prompt("You are a technical support specialist.")
        .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()])))
    .with_agent_spec(AgentDefinitionSpec::local_with_id("orchestrator", AgentDefinition::new("deepseek-chat")
        .with_system_prompt("Route user requests to the appropriate specialist:
- billing: payment, invoice, subscription issues
- support: technical problems, bugs, errors
Use agent_run to delegate. Use agent_output to read results.")
        .with_allowed_agents(vec!["billing".to_string(), "support".to_string()])))
    .build()?;
```

### Key decisions

- Worker agents should exclude delegation tools to prevent recursive delegation.
- The orchestrator prompt should list available agents and their responsibilities.
- Use foreground mode (`background=false`) when the user expects a direct answer.

## Sequential Pipeline

Multiple agents execute in order, each transforming the output of the previous stage.

```text
[Parser Agent] -> raw text
    -> [Extractor Agent] -> structured data
        -> [Summarizer Agent] -> final report
```

### Setup

The orchestrator prompt drives the sequence:

```rust,ignore
.with_agent_spec(AgentDefinitionSpec::local_with_id("orchestrator", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Process the document through three stages in order:
1. Call agent_run(\"parser\") with the raw input. Read output with agent_output.
2. Call agent_run(\"extractor\") with the parsed text. Read output with agent_output.
3. Call agent_run(\"summarizer\") with the extracted data. Read output with agent_output.
Return the final summary to the user.")
    .with_allowed_agents(vec!["parser".to_string(), "extractor".to_string(), "summarizer".to_string()])))
```

### Limitations

The orchestrator acts as a deterministic relay: call stage N, read output, pass to stage N+1. This relay logic does not require LLM reasoning, yet each relay step consumes an LLM turn. For long pipelines the token and latency overhead adds up. A future runtime-level sequential pipeline primitive could eliminate the relay agent entirely.

### When to avoid

If one agent with sequential tool calls can handle the full pipeline, do not split into multiple agents. Multi-agent adds latency and token cost. Split only when stages need different system prompts, tool sets, or model capabilities.

## Parallel Fan-Out/Gather

Independent tasks run concurrently, then a synthesizer combines results.

```text
                  ┌-> [Security Auditor]   -> security_report
[Orchestrator] ---┼-> [Style Checker]      -> style_report
                  └-> [Performance Analyst] -> perf_report
                           |
                     [Synthesizer] -> unified review
```

### Setup

The orchestrator prompt instructs the LLM to issue multiple `agent_run` tool calls in a single step. When the agent's tool execution mode supports parallel execution, the runtime runs them concurrently and returns all results before the next LLM turn.

```rust,ignore
.with_agent_spec(AgentDefinitionSpec::local_with_id("orchestrator", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Review the submitted code:
1. Launch all three reviewers in parallel by calling agent_run for each in the same response:
   - agent_run(\"security_auditor\")
   - agent_run(\"style_checker\")
   - agent_run(\"perf_analyst\")
2. Read the results from each tool call response.
3. Call agent_run(\"synthesizer\") with all three reports.
4. Return the unified review.
IMPORTANT: call all three agent_run tools at the same time, not one after another.")
    .with_allowed_agents(vec![
        "security_auditor".to_string(), "style_checker".to_string(), "perf_analyst".to_string(), "synthesizer".to_string()
    ])
    .with_tool_execution_mode(ToolExecutionMode::ParallelBatchApproval)))
```

The key is prompting the LLM to emit multiple tool calls in one response. Most capable models respect this instruction and produce parallel `agent_run` calls that the runtime executes concurrently.

### Limitations

Parallelism is **best-effort** with no runtime guarantee:

- The approach depends on the LLM choosing to emit multiple tool calls in a single response. If the model serializes them across turns, execution falls back to sequential.
- The runtime cannot force the LLM to produce parallel calls — it can only execute them concurrently when the LLM does.
- A future runtime-level fan-out primitive could guarantee parallelism independent of LLM behavior.

### Key decisions

- Set `tool_execution_mode` to `ParallelBatchApproval` or `ParallelStreaming` to enable concurrent tool execution.
- Prompt must explicitly instruct the LLM to call all agents at the same time.
- If reviewers depend on each other, use Sequential Pipeline instead.
- Keep each reviewer's tool set and prompt isolated so they can run independently.

## Hierarchical Decomposition

A parent agent breaks complex tasks into subtasks and delegates recursively.

```text
[Report Writer]
  |-> agent_run("researcher")
  |     |-> agent_run("web_search")
  |     |-> agent_run("summarizer")
  |-> agent_run("formatter")
```

### Setup

Middle-layer agents also have delegation tools:

```rust,ignore
.with_agent_spec(AgentDefinitionSpec::local_with_id("researcher", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Research the given topic. Use web_search for facts, summarizer for condensing.")
    .with_allowed_agents(vec!["web_search".to_string(), "summarizer".to_string()])))
.with_agent_spec(AgentDefinitionSpec::local_with_id("report_writer", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Write a report. Delegate research to the researcher agent, formatting to the formatter agent.")
    .with_allowed_agents(vec!["researcher".to_string(), "formatter".to_string()])))
```

### Key decisions

- Each layer only sees its allowed children, not the full agent tree.
- Leaf agents should exclude delegation tools entirely.
- Depth is unlimited but each level adds latency.

## Generator-Critic

The generator is the main agent. The critic is a child agent called via `agent_run`. The generator writes output, invokes the critic for review, and revises based on feedback — all within its own loop with full message history.

```text
[Generator] -> write draft
            -> agent_run("critic") with draft -> PASS or FAIL + feedback
            -> if FAIL: revise draft (history preserved)
            -> agent_run("critic") again
            -> repeat until PASS
```

This is better than an orchestrator-mediated approach because the generator retains its full conversation history across revision cycles. It sees all previous drafts and critic feedback naturally, without a middle agent relaying messages.

### Setup

```rust,ignore
.with_agent_spec(AgentDefinitionSpec::local_with_id("critic", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Validate the SQL query. Output exactly PASS if correct. \
        Otherwise output FAIL followed by specific errors.")
    .with_excluded_tools(vec!["agent_run".to_string(), "agent_stop".to_string()])))
.with_agent_spec(AgentDefinitionSpec::local_with_id("generator", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("You are a SQL query writer with a built-in review process:
1. Write a SQL query for the user's requirement.
2. Call agent_run for agent_id=critic with your query, background=false.
3. If critic returns FAIL, revise based on the feedback and call critic again.
4. Repeat until critic returns PASS or 5 attempts are reached.
5. Return the final validated SQL.
Always call the critic before finishing.")
    .with_allowed_agents(vec!["critic".to_string()])
    .with_max_rounds(20)))
```

### Key decisions

- The generator is the entry point (`agent_id` in `RunRequest`), not a separate orchestrator.
- The critic should exclude delegation tools to prevent recursion.
- Set `max_rounds` high enough to allow multiple generate-critique cycles.
- The critic must produce a clear pass/fail signal the generator can parse.
- Works best when validation criteria are objective (syntax, schema, format).

## Iterative Refinement

Extends Generator-Critic with a dedicated refiner child agent. The generator writes the initial draft, the critic reviews, and the refiner applies improvements based on feedback.

```text
[Generator] -> draft
            -> agent_run("critic") -> feedback
            -> agent_run("refiner") with draft + feedback -> improved draft
            -> agent_run("critic") again -> feedback or PASS
            -> repeat (max N iterations)
```

The setup follows the same pattern as Generator-Critic: the generator is the main agent with `allowed_agents: ["critic", "refiner"]`. Both critic and refiner are leaf agents without delegation tools.

## Human-in-the-Loop

High-risk operations pause for human approval before execution.

```text
[Agent] -> prepare transfer of $50,000
        -> tool suspends with SuspendTicket
        -> run transitions to Waiting
        -> human approves/denies
        -> run resumes or cancels
```

This pattern uses Tirea's built-in suspension model, not prompt engineering.

### Setup

See [Enable Tool Permission HITL](../how-to/enable-tool-permission-hitl.md) for the full configuration. The key components:

- `PermissionPlugin` intercepts tool calls and emits `Suspend` actions
- `ToolCallDecision` channel carries human decisions back into the loop
- Two inbound paths: decision forwarding (same run) and continuation (new run)

### Key decisions

- HITL is orthogonal to other patterns. Combine it with any pattern above.
- Use `parallel_streaming` tool execution mode for low-latency approval UX.
- See [HITL and Decision Flow](./hitl-and-decision-flow.md) for the full runtime model.

## Swarm / Peer Handoff

Agents operate as peers without a central coordinator. Each agent decides when to hand off to another. The handoff transfers full control on the same thread with shared message history.

```text
[Alice] <-> [Bob] <-> [Charlie]
  (math)    (code)    (writing)
```

Unlike Coordinator, there is no parent-child hierarchy. The active agent switches identity (system prompt, tools, model) in-place — no child spawning, no nesting.

### Mechanism

Tirea implements peer handoff via the `agent_handoff` tool and the `HandoffPlugin`:

1. Register multiple agents with `AgentOsBuilder` — each becomes a handoff target
2. When the LLM calls `agent_handoff("bob")`, the runtime writes a handoff request to thread state
3. On the next `before_inference` phase, `HandoffPlugin` applies the target agent's overlay: model override, system prompt, and tool restrictions
4. To return to the base agent, call `agent_handoff` again or use `clear_handoff_action()`

Handoff is **instant** — no run termination, no re-resolution, no new thread. The full conversation history is preserved.

### Setup

```rust,ignore
let os = AgentOs::builder()
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "alice",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are Alice, a math specialist."),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "bob",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are Bob, a code specialist."),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "charlie",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are Charlie, a writing specialist."),
    ))
    .build()?;
```

When 2+ agents are registered, the runtime automatically:
- Adds the `agent_handoff` tool to the tool set
- Computes handoff overlays from each agent's definition
- Wires the `HandoffPlugin` to enforce tool restrictions and apply overlays

Each agent can hand off to any other by calling `agent_handoff("target_id")`.

### Overlay fields

Each agent definition maps to a `HandoffRuntimeOverlay`:

| AgentDefinition field | Overlay effect |
|---|---|
| `model` | `OverrideModel` — switch the LLM model (empty = inherit base) |
| `system_prompt` | `AddSystemContext` — append to system prompt |
| `allowed_tools` | `IncludeOnlyTools` + hard gate in `before_tool_execute` |
| `excluded_tools` | `ExcludeTool` — hide specific tools |
| `fallback_models` | Fallback model chain for the override |

### Example: Plan mode via handoff

A "plan mode" restricts the agent to read-only tools for exploration before execution. Instead of a dedicated extension, define a planner agent:

```rust,ignore
let os = AgentOs::builder()
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "assistant",
        AgentDefinition::new("claude-sonnet")
            .with_system_prompt("You are a helpful coding assistant."),
    ))
    .with_agent_spec(AgentDefinitionSpec::local_with_id(
        "planner",
        AgentDefinition::new("")  // empty model = inherit from base agent
            .with_system_prompt(
                "Plan mode is active. Only use read-only tools to explore. \
                 When ready, call agent_handoff(\"assistant\") to return.",
            )
            .with_allowed_tools(vec![
                "Read".into(), "Glob".into(), "Grep".into(),
                "WebSearch".into(), "agent_handoff".into(),
            ]),
    ))
    .build()?;
```

The LLM calls `agent_handoff("planner")` to enter plan mode and `agent_handoff("assistant")` to exit. No framework-level plan extension is needed — the restriction is entirely configuration-driven.

### Key decisions

- Each agent's `allowed_tools` acts as a hard whitelist enforced at runtime, not just a prompt hint.
- `agent_handoff` is always allowed regardless of tool restrictions.
- An agent with empty `model` inherits the base agent's model — useful for mode switching without model changes.
- Handoff state is thread-scoped and persists across runs on the same thread.

### Why handoff instead of delegation

`agent_run` (delegation) spawns a child on a new thread. Handoff switches identity on the same thread:

| | Delegation (`agent_run`) | Handoff (`agent_handoff`) |
|---|---|---|
| Thread | New thread per child | Same thread |
| Message history | Isolated | Shared |
| Nesting | Grows per hop | Flat (no nesting) |
| Use case | Independent subtasks | Mode switching, peer routing |

## When to Use Multi-Agent

Split into multiple agents when:

- Stages need **different system prompts, tools, or models**
- A single context window **cannot hold** all required information
- **Parallel execution** provides meaningful speedup
- An independent critic role **improves output quality** over self-critique
- **Isolation** is needed (each agent sees only its allowed tools and data)

Stay with a single agent when:

- One agent with a few tools handles the full task
- The overhead of delegation (extra LLM calls, latency) outweighs the benefit
- The task is simple enough that prompt instructions suffice

## See Also

- [Sub-Agent Delegation](./sub-agent-delegation.md) for the runtime model behind delegation tools
- [Use Sub-Agent Delegation](../how-to/use-sub-agent-delegation.md) for delegation setup
- [Use Agent Handoff](../how-to/use-agent-handoff.md) for handoff setup and examples
- [HITL and Decision Flow](./hitl-and-decision-flow.md) for suspension mechanics
- [Architecture](./architecture.md) for the three-layer runtime model
