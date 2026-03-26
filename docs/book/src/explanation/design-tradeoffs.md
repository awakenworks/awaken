# Design Tradeoffs

This page documents key architectural decisions in Awaken and the tradeoffs they entail.

## Snapshot Isolation vs Mutable State

**Decision**: Phase hooks read from an immutable `Snapshot` and write to a `MutationBatch`. Mutations are applied atomically after all hooks converge.

**Alternative**: Hooks mutate shared state directly (protected by locks or sequential execution).

| | Snapshot Isolation | Mutable State |
|---|---|---|
| Correctness | Hooks see consistent state regardless of execution order | Result depends on hook ordering and lock granularity |
| Concurrency | Hooks can run in parallel without data races | Requires careful lock management or forced sequencing |
| Complexity | Requires `MutationBatch` machinery, conflict detection, merge strategies | Simpler implementation, direct reads and writes |
| Debuggability | Each phase boundary is a clean state transition | State changes interleaved, harder to trace |
| Cost | Extra `Arc` clone per phase for snapshot creation | No snapshot overhead |

**Why snapshot isolation**: Hook execution order should not affect correctness. When multiple plugins touch state in the same phase, mutable state creates implicit ordering dependencies that are difficult to test and reason about. The snapshot approach makes each phase a pure function from state to mutations, which is easier to verify and replay.

## Phase-Based Execution vs Event-Driven

**Decision**: Execution proceeds through a fixed sequence of phases (`RunStart` through `RunEnd`). Plugins register hooks at specific phases.

**Alternative**: Fully event-driven architecture where plugins subscribe to events and react asynchronously.

| | Phase-Based | Event-Driven |
|---|---|---|
| Predictability | Deterministic execution order within each phase | Non-deterministic ordering, race conditions possible |
| Plugin composition | Plugins interact at well-defined boundaries | Plugins interact through shared event bus, implicit coupling |
| Testability | Phase sequences are easy to unit test | Requires simulating async event flows |
| Flexibility | Adding behavior between phases requires new phases | New events can be added freely |
| Performance | Sequential phase execution adds overhead | Concurrent processing possible |

**Why phase-based**: Agent execution has a natural sequential structure (infer, execute tools, check termination). Phases formalize this structure and give plugins guaranteed execution points. Event-driven systems are more flexible but make it harder to reason about the order in which plugins see state and harder to implement features like "modify the inference request before it's sent."

## Typed State Keys vs Dynamic State

**Decision**: State keys are Rust types implementing the `StateKey` trait with associated `Value` and `Update` types.

**Alternative**: Untyped key-value store (`HashMap<String, Value>`).

| | Typed Keys | Dynamic State |
|---|---|---|
| Type safety | Compile-time guarantees on value and update types | Runtime type errors |
| Merge semantics | `MergeStrategy` declared per key at compile time | Merge logic must be external or convention-based |
| Discoverability | Keys are types -- IDE navigation, documentation | Keys are strings -- grep-based discovery |
| Boilerplate | Each key requires a type definition | Just use a string |
| Extensibility | New keys require code changes and recompilation | New keys can be added dynamically at runtime |

**Why typed keys**: State correctness is critical in an agent runtime. A mistyped key or wrong value type causes subtle bugs that surface only during execution. Typed keys catch these at compile time. The `apply` function makes update semantics explicit -- there is no ambiguity about how a counter is incremented or how a list is appended to.

## Plugin System vs Middleware Chain

**Decision**: Plugins register through `PluginRegistrar` and declare hooks, state keys, tools, and effect handlers as separate registrations.

**Alternative**: Middleware chain where each layer wraps the next (like tower middleware or HTTP middleware stacks).

| | Plugin System | Middleware Chain |
|---|---|---|
| Granularity | Hooks at specific phases, tools, state keys, effects -- each registered independently | Each middleware wraps the entire execution |
| Composition | Multiple plugins contribute hooks to the same phase | Middleware ordering determines behavior |
| Selective activation | `active_hook_filter` can enable/disable specific plugins per agent | Must restructure the chain to skip middleware |
| Complexity | More registration ceremony | Simpler mental model (wrap and delegate) |
| Cross-cutting concerns | Natural fit -- each plugin handles one concern | Each middleware handles one concern but sees all traffic |

**Why plugin system**: Agent execution has many extension points that don't nest cleanly. A permission check happens at `BeforeToolExecute`, observability spans wrap tool execution, reminders inject messages at `AfterToolExecute`. These are independent concerns at different phases. A middleware chain would require each middleware to understand the full lifecycle and decide when to act. The plugin system lets each plugin declare exactly which phases it cares about.

## Multi-Protocol Server vs Single Protocol

**Decision**: The server exposes AI SDK v6, AG-UI, A2A, and ACP endpoints simultaneously. Each protocol adapter translates `AgentEvent` into its wire format.

**Alternative**: Support a single canonical protocol and require clients to adapt.

| | Multi-Protocol | Single Protocol |
|---|---|---|
| Frontend compatibility | Works with AI SDK, CopilotKit, A2A clients out of the box | Requires custom adapter on the client side |
| Maintenance | Each protocol adapter must be kept in sync with `AgentEvent` changes | One adapter to maintain |
| Testing | Protocol parity tests ensure all adapters handle all events | Less test surface |
| Complexity | Multiple route sets, encoder types, and event mappings | One route set, one encoder |
| Runtime coupling | Runtime is protocol-independent -- only emits `AgentEvent` | Runtime could be coupled to the protocol |

**Why multi-protocol**: The AI agent ecosystem has not converged on a single protocol. AI SDK, AG-UI, and A2A serve different use cases (chat frontends, copilot UIs, agent-to-agent communication). Supporting multiple protocols at the server layer avoids forcing protocol choices on users. The `Transcoder` trait and stateful encoders keep the runtime decoupled -- adding a new protocol means implementing one encoder, not modifying the execution engine.

## See Also

- [Architecture](./architecture.md) -- three-layer design
- [State and Snapshot Model](./state-and-snapshot-model.md) -- snapshot isolation details
- [Run Lifecycle and Phases](./run-lifecycle-and-phases.md) -- phase execution model
- [Tool and Plugin Boundary](./tool-and-plugin-boundary.md) -- plugin vs tool design
