# ADR-0007: The Runtime Owns Tool Execution In-Process

- Status: Accepted
- Date: 2026-06-29
- Depends on: ADR-0001

## Context

The corpus framed tool execution as belonging to "the orchestration layer above":
hand tools (`read`, `glob`, `grep`, `bash`, `write`, `edit`, ...) were described
as client-executed or delegated, and a planned `ExecutionBackend` / `BackendProfile`
seam (guardrail G7) was to broker *where* agent and tool work runs.

The code never matched that framing. The loop executes a tool by calling
`RawTool::invoke` on an `Arc<dyn RawTool>` held in the runtime's own registry
(`engine.rs`); there is no delegation step, and `ExecutionBackend` /
`BackendProfile` / `ToolExecutor` have no references in `crates/*/src` at all.
Suspension is a gate decision (human-in-the-loop), never a mandatory tool hop.

The "execution lives above" premise therefore bought no capability — it only kept
the docs from describing what the runtime already does, and deferred shipping
real builtin tools behind a layer that does not exist.

## Decision

### D1: The runtime executes tools in-process

Tool execution is owned by the runtime. A registered tool's implementation runs
in the same process as the loop, invoked by id through the neutral `RawTool` port.
There is no "orchestration layer above" that owns tool execution placement.

### D2: Concrete tools live in the extension and do real work

Runtime core still ships no concrete model-callable tool id (ADR carried from
D14): the typed `Tool` / `RawTool` ports, descriptors, registry, gate, and commit
path are the only tool machinery in core. The official tools — including hand
tools that touch the filesystem — are concrete `Tool` implementations in
`awaken-ext-builtin-tools` that perform their IO directly. The extension owns the
ids and the behavior; core owns the ports.

### D3: `ExecutionBackend` / G7 is retired

The `ExecutionBackend` / `BackendProfile` agent-execution seam and its guardrail
G7 are removed. Remote or out-of-process agent execution is out of scope and
deferred to a future ADR that introduces it with a concrete driver and tests,
rather than carried as a vacuous Target. This does not touch the **model**
provider/model/backend binding (G22, `ModelBinding`): selecting which model server
answers a run is a separate concern and stays.

## Consequences

- The docs describe the system that exists: the runtime runs tools in-process.
- `awaken-ext-builtin-tools` gains executable tools (`read`/`glob`/`grep` first),
  not just descriptors; they register via `Runtime::with_tool` and run on call.
- G7 is tombstoned in [INVARIANTS.md](../INVARIANTS.md) (not renumbered, to keep
  G8–G32 references stable); D1/D6/D14 in
  [key-design-decisions.md](../design/key-design-decisions.md) drop the
  "execution above" clause; the design docs and wiki facts are swept accordingly.
- Genuinely remote agent execution must be re-proposed with its own ADR before it
  returns; until then the runtime assumes in-process execution.

## References

- [key-design-decisions.md](../design/key-design-decisions.md) — D1, D6, D14.
- [tool-and-capability.md](../design/tool-and-capability.md) — `Tool` / `RawTool`
  ports and the official builtin tools extension.
- [builtin-tools-extension-contract.md](../design/builtin-tools-extension-contract.md)
  — in-process hand-tool execution and the first vertical slice.
- [INVARIANTS.md](../INVARIANTS.md) — G7 (retired), G8 (capability segments).
