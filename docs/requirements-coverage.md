# Requirements Coverage

This document defines the complete `awaken-runtime` coverage target for this
repository. It imports the runtime-related design surface from the reference
corpus and classifies every adjacent server, product, environment, and analytics
requirement by how it may touch the runtime.

The runtime package owns execution semantics and durable runtime truth. Server,
distributed run ingress, config, hosted product, resource, and credential
systems consume runtime ports or provide opaque data to them; they do not define
runtime-domain behavior.

## Coverage Classes

| Class | Meaning |
|---|---|
| Core | Must exist in the `awaken-runtime` package and remain independent of server/product code |
| Runtime extension | Optional runtime package or extension crate that still obeys runtime guardrails |
| Server boundary | Belongs above runtime; may live in another package/repository and depends on runtime ports |
| Neutral platform boundary | Reusable control capability; runtime sees a port, id, or opaque reference |
| Hosted product boundary | Product specialization; depends on runtime through anti-corruption adapters |
| External/Product | Hosted or product-resource behavior; runtime receives only opaque refs or projections |
| Analytics/DX | Observability, eval, docs, examples, doctests, and e2e harnesses that prove behavior |

## Functional Coverage Matrix

| Requirement area | Class | Owning bounded context | Design source | Development rule |
|---|---|---|---|---|
| Runtime protocol and public contract license | Core | Runtime Core | [README license boundary](README.md#license-boundary), [packaging enforcement](design/packaging-enforcement-matrix.md), [invariants](INVARIANTS.md) | Protocol/specification, SDK-facing schemas, examples, and conformance tests use Apache-2.0; restricted layers cannot redefine runtime protocol terms |
| Agent runtime loop, typed state/tools, plugin hooks | Core | Runtime Core | [key design decisions](design/key-design-decisions.md), runtime behavior | Reuse `AgentRuntime`, `RunActivation`, `Tool`, `StreamSink`, `DurableEventSink`, `CommitCoordinator`; agent-domain truth stays separate from run ingress |
| Run phase and tool-call lifecycle | Core | Runtime Core | [runtime behavior](design/runtime-behavior.md) | Model phases as neutral runtime states; public statuses are projections |
| State keys, scoped state, actions, effects, snapshots | Core | Runtime Core + Stores | [runtime behavior](design/runtime-behavior.md) | Define key/scope/effect first; commit effects; rebuild snapshots from facts |
| Durable commit, facts, run/message invariants | Core | Runtime Core + Stores | [key design decisions](design/key-design-decisions.md#d12---contract-names-follow-authority), commit/fact-log guardrails | All durable agent-truth writes go through coordinator/store validation |
| Parallel tool writes and same-key conflicts | Core | Runtime Core | [runtime behavior](design/runtime-behavior.md) | Serialize or reject unsafe conflicts; no accidental last-write-wins |
| Serializable resolved run input, executable snapshots, catalog install, and catalog fingerprint | Core | Runtime Core + Config Edge | [config-to-run flow](design/config-to-run-execution-flow.md), [config publication lifecycle](design/config-publication-lifecycle.md) | Config domain coordinates publication and compiles registry data outside runtime; runtime accepts complete catalog install requests plus inline/by-id executable snapshots and builds live objects |
| Plugin activation, hook filtering, plugin tools | Runtime extension | Runtime Core | [runtime behavior](design/runtime-behavior.md), [tool and capability](design/tool-and-capability.md), [ADR-0004](adr/0004-plugin-factory-contributions-and-capability-bound.md) | Resolve to `Contributions` within a declared `CapabilityBound`; hooks cannot bypass capability, permission, or commit rules |
| Official builtin tools package | Runtime extension | Runtime Core extension + Environment seam | [tool and capability](design/tool-and-capability.md#official-builtin-tools-extension), [D14](design/key-design-decisions.md#d14---concrete-tool-ids-live-outside-runtime-core) | Runtime core provides no concrete tool ids; `awaken-ext-builtin-tools` supplies hand, task, and unified delegation toolsets |
| State-machine workflow extension | Runtime extension | Runtime Core | [runtime behavior](design/runtime-behavior.md) | Model workflow as state/action/effect/guard over committed facts |
| Context building and compaction | Runtime extension | Runtime Core | [runtime behavior](design/runtime-behavior.md) | Commit compacted summaries with lineage; replay reuses committed summaries |
| Cancellation and stop policies | Core/Server boundary | Runtime Core, Dispatch / Server | [runtime behavior](design/runtime-behavior.md), dispatch boundary | External cancel enters through ingress; runtime commits typed terminal reason |
| Runtime/server contract relayering | Core/Server boundary | Runtime Core, Dispatch / Server | [contract authority decision](design/key-design-decisions.md#d12---contract-names-follow-authority), runtime boundary design | Runtime exports a gated port; agent truth, run ingress, and protocol projection package separately when split |
| Direct runtime ingress and durable run ingress | Server boundary | Dispatch / Server | [runtime interface boundaries](design/runtime-interface-boundaries.md), [contract authority decision](design/key-design-decisions.md#d12---contract-names-follow-authority) | `DirectRunIngress` stays weak; `DurableRunIngress` adds durability without owning agent truth |
| Pending message, resume, handoff, sub-agent invocation | Core/Server boundary | Runtime Core, Dispatch / Server | [runtime behavior](design/runtime-behavior.md), run ingress boundary, [builtin tools](design/tool-and-capability.md#official-builtin-tools-extension) | Re-resolve at safe boundaries; pending input is durable server state until consumed; sub-agent invocation uses one `agent_run` tool with an `agent_id` argument |
| Scheduled actions, reminders, background work | Runtime extension + Server boundary | Runtime extension, Dispatch / Server | [runtime behavior](design/runtime-behavior.md) | Runtime emits scheduled/deferred effect; server owns durable wake and idempotent resume |
| Multi-protocol server adapters | Server boundary | Dispatch / Server | [protocol adapter boundaries](design/protocol-adapter-boundaries.md) | Server routes adapt to runtime; runtime does not import route state |
| HTTP API, SSE, errors, cancellation, config endpoints | Server boundary | Dispatch / Server + Product adapters | [config-to-run flow](design/config-to-run-execution-flow.md), [protocol adapter boundaries](design/protocol-adapter-boundaries.md), [error taxonomy](design/error-taxonomy.md) | Routes use `RunIngress`; public schemas are adapter projections |
| Admin console and docs site | Hosted product boundary | Product/server UI | product app design | UI consumes server APIs; not runtime core |
| Admin assistant tools | Hosted product boundary | Admin / Server | [D15](design/key-design-decisions.md#d15---admin-assistant-tools-are-server-owned), [tool boundary](design/tool-and-capability.md#admin-assistant-tool-boundary) | Admin-only tools live in `awaken-admin-assistant-tools` and a private registry; they are not ordinary builtin runtime tools |
| Admin audit log, drafts, publish/version switch, live prompt tuning | Server/Product | Admin / Product | config/admin coverage | Drafts and operator actions publish selected config through config-side services; runtime consumes a complete install result and pinned executable snapshot |
| ACP normalization | Neutral platform boundary | Tool/Protocol Adapter | ACP boundary design | ACP config/capability resolution fails closed |
| AI SDK, AG-UI, CopilotKit | Server/Hosted product boundary | Product protocol adapters | [protocol adapter boundaries](design/protocol-adapter-boundaries.md) | Map public streams/components to committed runtime events; no UI payloads in runtime core |
| Federated tool registries | Core/Neutral platform boundary | Runtime Core | federated tool design, [builtin tools](design/tool-and-capability.md#official-builtin-tools-extension) | Injection grants perception, not authorization; official tools enter through extension registries, not runtime-core defaults |
| Goal evaluation / outcomes | Runtime extension + Hosted product boundary | Runtime extension, product adapter | run and eval design | Runtime records opaque verdicts; product maps public outcome names |
| Observability, trace persistence, dataset capture | Analytics/DX | Projection / Analytics | [runtime behavior](design/runtime-behavior.md), [observability/eval boundary](design/observability-eval-dataset-boundary.md) | Consume committed facts/events; traces are not runtime truth |
| Eval execution, judges, experiment routing | Analytics/DX | Eval / Server | [runtime behavior](design/runtime-behavior.md), [observability/eval boundary](design/observability-eval-dataset-boundary.md) | Run through the same runtime ports; mock providers and judges are adapters |
| Capability segmentation | Core/Neutral platform boundary | Runtime Core, Config Domain, Environment | capability segmentation design | Pin descriptors and hashes; execute by id; keep policy/secrets separate |
| Permission engine | Runtime extension | Runtime Core | [permission policy axis](design/permission-policy-axis.md) | Authorization only through permission path |
| Model/provider routing | Core/Server boundary | Runtime Core + Config Domain | [model/provider/backend binding](design/model-provider-backend-binding.md) | Resolve named refs; reconcile capabilities; no grant from compatibility |
| Deferred tools | Runtime extension | Runtime Core | deferred-tool design | Use `ToolSearch`/DiscBeta; no separate discovery mechanism |
| Skills | Runtime extension + Hosted product boundary | Runtime Core, Config Domain, Environment | skills and resource design | Reuse parser/registry; product owns public skill/version API and mounts |
| MCP tools | Runtime extension + Hosted product boundary | Runtime Core + Product adapter | MCP integration design | Reuse `awaken-ext-mcp`; product maps config/credentials |
| Hosted public DTO/API surface | Hosted product boundary | Product / Config/Admin | [protocol adapter boundaries](design/protocol-adapter-boundaries.md) | DTOs and event names stop at anti-corruption bridge |
| Sessions, session events, SSE resumption | Hosted product + Server boundary | Product adapter, Dispatch / Server | [protocol adapter boundaries](design/protocol-adapter-boundaries.md), [commit/fact/projection taxonomy](design/commit-fact-projection-taxonomy.md) | Public sessions are projections over committed runtime/server state |
| Session threads / multiagent | Hosted product boundary | Product adapter + Runtime thread model | session coverage | Map to thread hierarchy and projections; keep authority in store facts |
| Hosted agent definitions and adapter profiles | Hosted product / Neutral platform boundary | Config Domain | hosted definition design | Kind-discriminated config validates axes at store boundary |
| Credentials, vaults, accounts, availability | Hosted Product / External | Credential Domain, Product data plane | credential lifecycle design | Runtime sees opaque refs; selection/probe is not authorization |
| Memory stores | External/Product | Product resource data plane | [resources design](design/resources-memory-files-skills.md) | Dedicated component; integrate by logical references |
| Files/resources | External/Product | Product resource data plane | [resources design](design/resources-memory-files-skills.md) | Paths never cross runtime boundaries |
| Shared state and product state stores | External/Product + Environment | Product data plane, Runtime ports | [resources design](design/resources-memory-files-skills.md), [runtime behavior](design/runtime-behavior.md) | Shared/product state is addressed logically and synchronized through approved ports |
| File, SQLite, PostgreSQL, NATS, and schema management | Server/Neutral Platform | Store adapters / Operations | deployment and storage coverage | Store adapters satisfy the same contracts; backing-service choice does not change domain behavior |
| Webhooks | Hosted Product / External | Product projection | [commit/fact/projection taxonomy](design/commit-fact-projection-taxonomy.md) | Webhooks are projection sinks over committed facts/outbox |
| Resilience and typed failure/non-progress | Core/Neutral platform boundary | Runtime, Dispatch, Control | [error taxonomy](design/error-taxonomy.md) | Failures are typed and replay/resume-safe |
| Doctests, tutorials, examples, testing strategy | Analytics/DX | Developer Experience | docs/tutorial/e2e coverage, [runtime scenarios](design/runtime-scenario-validation.md) | Every public pattern should map to one design row and one executable example or test |
| E2E harnesses and protocol conformance | Analytics/DX | Test harness / Product adapters | [runtime scenarios](design/runtime-scenario-validation.md), e2e and protocol coverage | P0/chaos/protocol scenarios assert boundaries, projection order, and adapter compatibility |
| Unified binary/role subcommands | Server/Neutral platform boundary | Assembly | role-targeted assembly design | Same code path, role-targeted assembly |
| Runtime package split | Core packaging | Runtime Core | runtime packaging design | Server/product crates move out but consume same runtime port |

## Reference Family Audit

Use this table to check that a crate, guide, how-to, or protocol reference has a
home in the matrix above without exposing local reference-project paths.

| Reference family | Covered by matrix rows |
|---|---|
| Runtime facade, runtime, runtime contracts, tool pattern, agent helper crates | Agent runtime loop; run lifecycle; state/effects; runtime/server contract relayering |
| Dispatch, run ingress, server, server daemon, stores, SQL schema management | direct/durable ingress; HTTP/SSE/config endpoints; store adapters; unified binary |
| Builtin, permission, deferred tools, goal, state-machine, context, scheduled/reminder, observability extensions | official builtin tools package; permission engine; deferred tools; goal evaluation; state-machine; context compaction; scheduled work; observability/eval |
| Skills, MCP, A2A, ACP, remote backends, hosted remote kinds | skills; MCP tools; A2A/remote kinds; ACP normalization; backend capability checks |
| Hosted contract/API/bridge, sessions, webhooks, public events | hosted public DTO/API surface; sessions/SSE; webhooks; anti-corruption adapters |
| File, memory, shared state, product resources, credentials/vaults | memory stores; files/resources; shared state; credentials/vaults; resource realization |
| Admin console, admin assistant tools, config drafts, audit, experiments, eval history | admin console; `awaken-admin-assistant-tools`; admin audit/drafts/publish; observability/eval; experiment routing |
| AI SDK, AG-UI, CopilotKit, HTTP clients | multi-protocol adapters; protocol adapter boundaries; public API projections |
| Tutorials, docs, doctests, examples, e2e harnesses | doctests/tutorials/examples; e2e/protocol conformance; developer experience |

## Whole-Requirement Acceptance

The corpus is complete enough to guide runtime development only if every change
can be placed in one row above and one bounded context in
[design/architecture-overview.md](design/architecture-overview.md). If a feature
does not fit, update this coverage map before implementation.

## Packaging Rule

The runtime package must remain independently publishable. Server, distributed
config, hosted product, and app code may live in adjacent packages or
separate repositories, but they consume the same runtime ports and must not push
product, deployment, publication-coordination, or registry-compilation
vocabulary into runtime-owned code.

No requirement is allowed to cross a domain boundary just because the packaging
mode changes.
