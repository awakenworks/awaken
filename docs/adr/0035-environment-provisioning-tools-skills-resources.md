# ADR-0035: Environment Provisioning — One Seam for Tools, Skills, and Resources

- Status: Accepted
- Date: 2026-07-02
- Depends on: ADR-0007 (runtime owns tool execution), ADR-0034 (runtime axis
  model — D5 presentation-only fingerprint, D6 kernel is placement/binding
  agnostic)
- Relates to: `design/resources-memory-files-skills.md` (owner of the
  resource/skill boundary), `design/tool-and-capability.md`,
  `design/architecture-overview.md`

## Context

ADR-0034 D6 fixed that per-environment isolation is a **host composition
choice — a per-environment tool set — never a kernel feature**: a tool's
execution location is decided by *which `RawTool` the host composes into the
run*, and the kernel invokes tools by id, placement-agnostic. The local
provider already realizes this: `awaken-sandbox-local` exposes
`SandboxProvider::create(&SandboxSpec) -> Environment`, and `Environment`
hands the host a `Vec<RawTool>` (today only jailed hand tools) "without knowing
*where* they execute" — local yields rooted in-process tools, a distributed
provider yields relay tools bound to a container.

Four capabilities the corpus still owes an execution home — **skills**,
**MCP tool sets**, **files/resources**, and **agent-authored skill iteration**
— all share the same shape as tools: they are materialized *per run* in a way
that is *sandbox-specific*, and they surface to the kernel either as executable
capabilities (`RawTool`) or as opaque realized references consumed by tools.
Nothing about them is kernel logic.

This ADR records the decision to treat **provisioning** — `SandboxProvider::
create` — as the single seam that materializes the whole per-run capability
substrate, and to keep delivery, versioning, and collection of skills entirely
outside the runtime. It does not introduce a kernel subsystem; ADR-0034 D6
already forbids that. It generalizes the `Environment` capability surface and
names the external components and the local/managed adapter split.

## Decision

### D1: Provisioning is `SandboxProvider::create`; it materializes the whole per-run substrate

One seam — `SandboxProvider::create(&SandboxSpec) -> Environment` — materializes
everything the agent's execution needs that is not kernel logic: isolation
(`IsolatedRoot`), hand/builtin tools, skill tools, MCP tool sets, and realized
file/resource references. `SandboxSpec.mounts` / `constraints` (already reserved
as forward-compatible data) carry the declared provisioning inputs (skill refs,
resource refs, mount descriptors). This extends ADR-0034 D6's "per-environment
tool set" to a **per-environment provisioned capability set**.

### D2: The kernel stays environment-agnostic — zero change

The kernel consumes a `Vec<RawTool>` plus opaque realized refs and nothing more.
It never names a skill, a mount, a provider, a provenance, or a placement. A
provisioned tool cannot author runtime truth: the only durable write boundary is
the `CommitCoordinator` (G13), and environment-bound tools are given no commit
path — the perception/authorization split is structural, whether the tool runs
in-process or via relay. `awaken-runtime` and `awaken-runtime-contract` require
no new types for this ADR.

### D3: Two producers of run input, kept separate

A run's input has two producers that must not merge (ADR-0034 D2, D5):

| Producer | Owns | Currency | Pin |
|---|---|---|---|
| Configuration (`compile`) | *what the agent is* — instructions, model binding, config-derived tool **presentation** | serializable `ResolvedSpec` + `CatalogFingerprint` (G3) | kernel fingerprint (presentation only, D5) |
| Environment (`provision`) | *where/how it runs* — isolation, provisioned capabilities, realized resources | `Environment` (live `RawTool`s + opaque refs) | **host-side provisioning receipt** |

The environment half is pinned by a **provisioning receipt**
(`{ref, version, content_hash}[]`), recorded host-side with the run and re-provisioned
on replay. It is not folded into the kernel's `CatalogFingerprint`, which by
D5 covers only the model-visible presentation. Full run identity =
`CatalogFingerprint` ⊕ receipt hash, composed above the kernel.

### D4: Skills are environment-provisioned `RawTool`s, not a kernel or runtime-ext concept

> **Superseded by [ADR-0036](0036-skills-as-runtime-extension-single-tool.md).**
> The per-skill `RawTool` shape below (one dynamic tool per skill) is withdrawn: skills are now
> fronted by a single `Skill` tool with the catalog carried as data, implemented
> in the `awaken-ext-skills` extension (still outside the kernel). D1–D3 and
> D5–D8 of this ADR are unaffected — provisioning still materializes resources
> and the per-run substrate.

A skill surfaces to the kernel as a dynamic `RawTool` (the `awaken-ext-mcp`
contribution template: `dynamic_tools` under a `tool_namespaces` ceiling, G30).
Progressive disclosure is native: the tool descriptor carries `name` +
`description` (always visible); calling the tool loads the `SKILL.md` body and
returns it as a tool result the kernel injects as context. Visibility filtering
(`disable-model-invocation` and runtime hide/show) happens in the provisioner —
it simply does not provision a hidden skill, or provisions it demoted. There is
no `awaken-ext-skills` runtime extension doing in-kernel discovery, rendering, or
visibility.

### D5: Delivery and collection are external; the runtime is out of the loop

- **Skill Store** (control plane, external): immutable content-addressed
  versioned skills with `parent` lineage, `head` per channel, and provenance.
  The provisioner *pulls* selected versions from it.
- **Skill Collector** (external): observes produced artifacts (workspace files
  the agent wrote via ordinary file tools, or the provisioner's `harvest`
  output), validates, content-addresses, and *publishes* new versions through a
  `PromotionGate`. Whether the agent "self-updated" is never a runtime concern —
  the runtime only ever emitted ordinary file effects.

Both talk to the provisioner, never to the kernel. A missing collector does not
change runtime behavior.

### D6: Trust boundary and authorization stay outside provisioning

The delivered skill mount is **read-only** and control-owned (trusted, pinned);
the agent's **workspace** is writable and its output is untrusted until it
crosses the `PromotionGate`. These are two separate roots (expressible today as
distinct `IsolatedRoot`s). Provisioning grants **perception, not
authorization** (G9/G21): the permission policy remains the only grant path, and
authorizes each skill tool call and each tool a skill invokes; a skill's
`allowed_tools` is a selection over already-granted tools, enforced at the gate.
The `PromotionGate` is a separate authorization for publishing to the shared
store — an agent may propose, not unilaterally publish.

### D7: Local and Managed are two `SandboxProvider` adapters of one port

`LocalSandboxProvider` (rooted in-process tools) and a hosted/managed provider
(relay tools bound to a container/VM, with lease / warm-pool / supervisor
lifecycle behind the same port) are two adapters of one `SandboxProvider`
abstraction. Switching local ↔ managed swaps the provider; the kernel is
unchanged. This is distinct from the **managed protocol** front door
(`awaken-protocol-managed`, the public wire ↔ runtime anti-corruption adapter):
a managed agent = managed protocol (front door) + managed Environment
(execution substrate). The two are separate axes that compose. Warm reuse is an
optimization; cold replay from the provisioning receipt stays the correctness
baseline.

### D8: The one refactor — generalize `Environment` to a capability surface

`Environment` is broadened from hand-tools-only to a full capability surface: a
`tools()` accessor returning hand + skill + provisioned dynamic tools as
`Vec<RawTool>`, plus accessors for realized resource references. This is
additive and contained in `awaken-sandbox-local` and its host call sites; no
kernel or contract change. Credentials are provisioned as opaque refs only —
their values never enter the `Environment` surface, the receipt, or any
fingerprint.

## Consequences

- Skills, MCP, files, and resources share one execution home (the Environment),
  one seam (`provision`), and one pin (the host-side receipt) — no per-capability
  runtime subsystem.
- The runtime-change surface is **zero** (ADR-0034 D6 extended): a skill/MCP/
  resource capability is a provisioned `RawTool` or ref; the kernel never learns
  a new concept. The only code movement is the `Environment` generalization (D8)
  plus new external crates (skill store, collector) and provider adapters.
- Local and managed execution differ only by which `SandboxProvider` the host
  installs; delegation/sub-agent wiring already routes through this seam.
- Agent-authored skill iteration is supported without any "self-update" runtime
  concept: ordinary file effects out, external collection and promotion back,
  delivery on the next dispatch.
- Governance is layered and all outside the kernel: `tool_namespaces` bounds
  what can be authored (G30), `PromotionGate` bounds what can be published, and
  the permission policy bounds what can be used (G9/G21).

## Non-Goals

- **Capability-based execution routing is not a provisioning concern.** A
  routing/trust taxonomy (e.g. `Capability{Action, Perception, Trusted}`, as
  awaken-next tags executor requests) has two separable facets that belong to
  two different layers, and neither is the `provision` seam:
  - the **classification** (is a call action / perception / trusted?) is
    descriptive tool metadata — a descriptor / capability-segment overlay applied
    at resolve ([ADR-0034](0034-runtime-axis-model-and-orthogonality.md) D1
    overlay dimensions, [tool-and-capability.md](../design/tool-and-capability.md));
    the provider is merely one producer of that descriptor for dynamic tools;
  - the **routing decision** that consumes it (which locus/executor runs the
    call, how it is isolated, dedupe/exactly-once) is a dispatch / placement
    concern that lives **above** both the kernel and this seam — realized by host
    composition (ADR-0034 D6) or a future `ToolExecutor` cross-cutting policy,
    never as a field on `SandboxSpec`, `Environment`, or the kernel.

  Adding such a tag to the provisioning seam would relayer a dispatch decision
  into provisioning. It is deferred (YAGNI) until a concrete multi-locus routing
  requirement exists (local in-process vs remote sandbox vs read replica); when
  it does, it is recorded in a dispatch/executor ADR, consistent with ADR-0034
  D6 keeping `ToolExecutor` unused until a cross-cutting execution policy needs
  it. The current trust boundary (read-only mount, G13, permission gate) does not
  depend on it.

## Alternatives considered

- **An `awaken-ext-skills` runtime extension that discovers/renders/gates skills
  in-process** (the goal / awaken-next shape). Rejected: it makes the kernel (or
  a kernel-adjacent extension) know "skill", duplicating tool composition,
  visibility, and IO that ADR-0034 D6 already assigns to host/environment
  composition. Skills-as-provisioned-tools reuse the `RawTool` seam with no new
  concept.
- **Fold the provisioned skill set into the kernel `CatalogFingerprint`.**
  Rejected (D3, ADR-0034 D5): the kernel fingerprint covers presentation only;
  environment provisioning is pinned above the kernel by the receipt. Folding it
  in would push placement/provenance into the kernel, which D6 forbids.
- **A dedicated skill delivery path separate from the sandbox provider.**
  Rejected (D1): delivery *is* provisioning; a second path would duplicate the
  `SandboxSpec`/`Environment` seam and split "how a sandbox materializes things"
  across two mechanisms.
- **A separate managed Environment port distinct from `SandboxProvider`.**
  Rejected (D7): managed is another adapter of the same port; a second port would
  fork the host composition and lose the "swap the provider, kernel unchanged"
  property.

## References

- [ADR-0034](0034-runtime-axis-model-and-orthogonality.md) — D5 (presentation-only
  fingerprint), D6 (kernel is placement/binding/relay-agnostic; per-environment
  tool set is host composition).
- [ADR-0007](0007-runtime-owns-tool-execution.md) — where a call runs is a
  `RawTool` detail.
- [resources-memory-files-skills.md](../design/resources-memory-files-skills.md)
  — owner of the resource/skill boundary (no absolute path as authority; cold
  replay is the baseline; environment realizes resources).
- [tool-and-capability.md](../design/tool-and-capability.md) — capability
  segments and the neutral `ToolExecutor`/`RawTool` ports.
- [INVARIANTS.md](../INVARIANTS.md) — G3, G4, G9, G13, G21, G30.
