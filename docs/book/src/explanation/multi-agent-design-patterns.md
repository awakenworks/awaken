# Multi-Agent Design Patterns

This page describes common multi-agent patterns and how they map to Tirea primitives.

All patterns below are implemented with the same building blocks:

- `agent_run` / `agent_stop` / `agent_output` delegation tools
- `AgentDefinition` with `allowed_agents` / `excluded_agents`
- `SuspendTicket` for human-in-the-loop gating
- System prompt engineering for control flow

Tirea does not require dedicated workflow agent types. The LLM-driven loop plus delegation tools cover these patterns through prompt configuration.

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
| [Swarm / Peer Handoff](#swarm--peer-handoff) | **TODO** — not yet supported | — |

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
    .with_agent("billing", AgentDefinition::new("deepseek-chat")
        .with_system_prompt("You are a billing specialist.")
        .with_excluded_tools(vec!["agent_run", "agent_stop"]))
    .with_agent("support", AgentDefinition::new("deepseek-chat")
        .with_system_prompt("You are a technical support specialist.")
        .with_excluded_tools(vec!["agent_run", "agent_stop"]))
    .with_agent("orchestrator", AgentDefinition::new("deepseek-chat")
        .with_system_prompt("Route user requests to the appropriate specialist:
- billing: payment, invoice, subscription issues
- support: technical problems, bugs, errors
Use agent_run to delegate. Use agent_output to read results.")
        .with_allowed_agents(vec!["billing", "support"]))
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
.with_agent("orchestrator", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Process the document through three stages in order:
1. Call agent_run(\"parser\") with the raw input. Read output with agent_output.
2. Call agent_run(\"extractor\") with the parsed text. Read output with agent_output.
3. Call agent_run(\"summarizer\") with the extracted data. Read output with agent_output.
Return the final summary to the user.")
    .with_allowed_agents(vec!["parser", "extractor", "summarizer"]))
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
.with_agent("orchestrator", AgentDefinition::new("deepseek-chat")
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
        "security_auditor", "style_checker", "perf_analyst", "synthesizer"
    ])
    .with_tool_execution_mode(ToolExecutionMode::ParallelBatchApproval))
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
.with_agent("researcher", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Research the given topic. Use web_search for facts, summarizer for condensing.")
    .with_allowed_agents(vec!["web_search", "summarizer"]))
.with_agent("report_writer", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Write a report. Delegate research to the researcher agent, formatting to the formatter agent.")
    .with_allowed_agents(vec!["researcher", "formatter"]))
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
.with_agent("critic", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("Validate the SQL query. Output exactly PASS if correct. \
        Otherwise output FAIL followed by specific errors.")
    .with_excluded_tools(vec!["agent_run", "agent_stop"]))
.with_agent("generator", AgentDefinition::new("deepseek-chat")
    .with_system_prompt("You are a SQL query writer with a built-in review process:
1. Write a SQL query for the user's requirement.
2. Call agent_run for agent_id=critic with your query, background=false.
3. If critic returns FAIL, revise based on the feedback and call critic again.
4. Repeat until critic returns PASS or 5 attempts are reached.
5. Return the final validated SQL.
Always call the critic before finishing.")
    .with_allowed_agents(vec!["critic"])
    .with_max_rounds(20))
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

> **TODO** — This pattern is not yet supported. The section below describes the target behavior and current gap.

Agents operate as peers without a central coordinator. Each agent decides when to hand off to another. The handoff transfers full control: Agent A exits, Agent B takes over the same thread with shared message history.

```text
[Alice] <-> [Bob] <-> [Charlie]
  (math)    (code)    (writing)
```

Unlike Coordinator, there is no parent-child hierarchy. The active agent switches identity, not spawns a child.

### Why delegation does not work for handoff

Tirea's current `agent_run` model is hierarchical (parent spawns child). Using it for handoff has fundamental issues:

- **Nesting depth grows per hop.** A→B→C means C runs as a grandchild. Each hop adds a level of indirection and resource consumption.
- **No shared message history.** Each child agent gets its own thread. The handoff target cannot see the prior conversation.
- **Parent does not exit.** The parent agent remains alive waiting for the child to finish, consuming context window.

A true peer handoff requires a different mechanism: the runtime switches the active agent's identity (system prompt, tools, model) on the same thread within the same run. This is not yet implemented.

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
- [Use Sub-Agent Delegation](../how-to/use-sub-agent-delegation.md) for setup instructions
- [HITL and Decision Flow](./hitl-and-decision-flow.md) for suspension mechanics
- [Architecture](./architecture.md) for the four-layer runtime model
