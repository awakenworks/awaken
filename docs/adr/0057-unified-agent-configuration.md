# ADR-0057: Unified Agent Configuration — One Kind-Discriminated Aggregate for Native/ACP/A2A, Three Orthogonal Axes (Executor / Model Dialect / Vendor), Vendor-Derived Credential Pools; Config Authors Intent, Runtime Sees Only a Secret-Free `backend_ref`

- Status: Proposed
- Date: 2026-07-16
- Amended 2026-08-01 by [ADR-0073](0073-session-environment-owned-hand-and-worker-capability-placement.md):
  the proposed `hand` Agent field, `declared_hand`, deployment
  `hand_connections`, and `ToolExecutorProvider` bridge are retired. Environment
  and durable Worker placement are the only execution authorities.
- Amended 2026-07-30: each declarative `AcpCli` row owns its supported model API
  dialect tokens. The resolver-side CLI→dialect table and its parallel error
  vocabulary are retired; publication intersects an Offering with that one
  capability catalog.
- Implemented foundation (2026-07-22): `/v1/agents` and
  `/v1/config/agents` now adapt the same Workspace-scoped `AgentConfig`
  aggregate in ConfigPlane. Managed writes use revision CAS, immutable history is
  persisted by `awaken.config` Scoped Migration v9, archive is an aggregate
  lifecycle state, and archived Agents are removed from/rejected by the execution
  projection. The broader kind/dialect/vendor model in this ADR remains proposed.
- Builds on: the config/credential/secret seam and the runtime-unaware secret rule
  (ADR-0043); the resolved-spec-is-the-only-runtime-surface boundary (ADR-0031,
  `ResolvedSpec`/`ExecutableAgentSnapshot`); the typed `Backend::from_ref`
  routing axis (`awaken-runtime-contract::resolved`); the model catalog's
  `ApiDialect`/`Provider`/`Offering` (`awaken-model-catalog`); the credential
  plane's `CredentialSource`/`CredentialPool`/`CredentialBinding`/`SelectionPolicy`
  + `AvailabilityLedger` (`awaken-credential-vault`); the ACP catalog `AcpCli` and
  projecting sources (ADR-0041/`awaken-run-executor-acp`); the brain–hand relay,
  connection plan, and hand placement (ADR-0044/0045/0046); the A2A peer executor
  and cross-tenant carve-out (ADR-0048/0049); telemetry content-capture consent
  and GDPR erasure (ADR-0050); the management assistant as an ordinary agent in a
  reserved scope (ADR-0052).
- Reference: the unified-agent-configuration model in `~/Codes/awaken-next`
  (ADR-0117 there): a kind-discriminated `ManagedAgentDefinition` with orthogonal
  axes and a store-backed `AdapterProfile`. A reference architecture, not a code
  dependency — this ADR keeps this repo's stronger invariant that no credential
  ever enters the runtime snapshot.

## Context

An agent in this repo is authored as a single flat `AgentConfig`
(`awaken-config-store::config`) whose *execution kind* is smuggled into the
`model_binding.backend_ref` string (`"genai"` → native, `"acp:<cli>"` → external
CLI, `"a2a:<endpoint>"` → remote). The struct is native-shaped: `instructions`,
`tools`, `context_policy`, `plugin_config` are first-class, while `skills`,
`mcp_servers`, and `multiagent` are opaque `Vec<serde_json::Value>` authoring
passthrough that the runtime never consumes. ACP-specific knobs ride inside
`plugin_config["acp"]` string paths; a remote agent has no config struct at all
(endpoint lives in the `backend_ref` string, auth is absent on the
`A2aRunExecutor::over_http()` path); whether a native agent runs with a Hand is
decided entirely by host-side placement wiring and is invisible in config.

This produces four recurring defects:

1. **Kind is stringly-typed.** Nothing validates that an `acp:codex` agent is
   fed a config an `acp:codex` runtime can honor; nothing declares whether a
   given kind *supports* skills/MCP, so an unsupported declaration fails silently
   at runtime instead of at publish.
2. **Two concepts are conflated as "dialect".** The *ACP runtime* (which external
   CLI) and the *model API dialect* (`AnthropicMessages`/`OpenAiChat`/`Gemini`)
   are independent, but the code and prior drafts treated `acp:claude` as if it
   *were* the dialect. It is not: an `acp:claude` runtime can be pointed at a
   MiniMax-vendor endpoint speaking the Anthropic-messages dialect.
3. **Credential matching is an implicit, unchecked coincidence.** The resolver
   picks "the workspace's first Active credential that `can_consume` the model's
   provider" (`config_executor.rs`); coherence between the model's provider, the
   ACP runtime's expected env family, and the credential is nobody's asserted
   invariant.
4. **The per-agent CLI, Hand, and remote-auth capabilities exist but are not
   wired** into the production authoring→execution path (per-agent ACP projection
   is only in scenario hosts; A2A auth only in hand-constructed transports).

We want one authoring aggregate that expresses **Native (± Hand) / ACP / A2A** as
first-class kinds, declares skill/MCP support per kind, keeps the common config
consistent across kinds, supports every ACP mode and custom knob, defines the
model dialect explicitly (distinct from the runtime) and matches it to vendors,
and auto-pools a vendor's same-type credentials — all without weakening the
runtime-sees-no-secret boundary.

## The three orthogonal axes (ubiquitous language)

Every name below is closed to its axis and must never borrow another axis's word.

| Axis | Authoritative name | Intent | Values | Decided by | Must NOT mean |
|---|---|---|---|---|---|
| ① Executor | `AgentKind` + `AcpCliId` | *who runs the agent* (native loop / which external CLI / remote) | `Native`/`Acp`/`A2a`; CLI = `claude`/`codex`/`gemini`/`opencode` | projected to `backend_ref` | not the model, not the vendor |
| ② Model dialect | `ApiDialect` | the wire protocol *to the model API* | `AnthropicMessages`/`OpenAiChat`/`Gemini` | resolved `Offering.dialect` | **not "which CLI"**, not the vendor |
| ③ Counterparty | `provider_id` (model-path prose: *vendor*) | *whoever the credential authenticates to*; whose key/quota | vendor slug (`anthropic`/…) or endpoint origin | `Offering.provider_id` (models) / endpoint origin (remote) | not the dialect, not the CLI |

A self-consistent triple (also the current test reality): `cli=claude` ×
`dialect=AnthropicMessages` × `vendor=minimax` — three axes each valid, none
derivable from another.

Naming discipline clauses:

- `AcpCliId` (axis ①) says "which external binary". It is deliberately not named
  `dialect` and not named `runtime` (avoids collision with `awaken-runtime`).
- `ApiDialect` (axis ②) keeps the existing model-catalog name; the `Api` prefix
  asserts "this is the *model API* dialect", not an agent protocol or a CLI.
- axis ③'s field is the existing `provider_id`, read as **counterparty** —
  whoever the credential authenticates to (a catalog vendor slug on the model
  path, an endpoint origin on the A2A path); *vendor* is its model-path
  specialization. Credentials match on counterparty only.

## Concept unification — one authoritative home per concept per context

Unification here does **not** mean one global type. The same concept legitimately
takes different shapes in different bounded contexts (that is what a context map
is for); what this ADR fixes is (a) exactly **one authoritative expression per
concept per context**, and (b) **explicit, tested projections** between contexts.
The defect test: duplication *within* a context is redundancy to remove;
projection *across* contexts is architecture to keep.

| # | Family | Authoritative concept | What it unifies | Cross-context projections |
|---|---|---|---|---|
| 1 | Selection axis | `AxisBinding` semantics: Pin/Pool + `SelectionPolicy` + `AvailabilityLedger` | model axis (`ModelSelection`+`model_fallbacks`) and credential axis (`CredentialBinding` + the resolver's default vendor-pool derivation) are two instances of one shape | `Auto` collapses to Pin at publish; the default derivation expands to a Pool at resolve. No fourth shape may be added |
| 2 | Executor axis | one concept, three context forms | `AgentKind` (authoring) ⇌ `backend_ref` (published language) ⇌ `Backend` (runtime ACL) → `DispatchRunExecutor` routing | projection + parse, round-trip tested; `LaunchSource` cli-match re-asserts it at worker open |
| 3 | Model speech | `Offering.dialect` is the selected route's dialect source; each `AcpCli` row owns the stable protocol tokens it can consume | `ApiDialect` (control-plane wire protocol), `AcpCli::model_api_dialects` (executor capability tokens), `ModelDelivery` (credential delivery), vendor (`provider_id`, ⊥ dialect) | publication intersects the Offering token with the selected executor row; vendor↔credential is checked independently; `ResolvedModel` is the runtime terminal form |
| 4 | Credential lifecycle | declare → select → materialize → realize at the permitted boundary | `CredentialSource` (secret-free row) → family 1 selection → pinned realization → model client, process-secret broker, or mediated transport; ADR 67 removes the former ACP `McpCredential` reference/inline exits | selection collapses to ONE path (`resolve_credential`); ACP receives no MCP credential channel; `config_executor` inline `.find` retired |
| 5 | External dependency auth | ONE declare table: `CredentialSource` keyed by *counterparty* (vendor slug or endpoint origin); optional refinements `McpServerDef` (referenced) / `InferenceProfile` (model override) | plane-local MCP projections stay (runtime plugin, ACP `SessionMcpServer`, managed wire); the two overlapping protocol-managed shapes collapse to one | declare → select → materialize → inject (only the exit differs per kind) |
| 6 | Session continuity (ACP) | intent × mechanism × facility | intent = `SessionReuse`/`session_mode`/`compact_window` (config); mechanism = `SessionPersistence`/`ModelSwitch::Relaunch` (catalog row); facility = `ConfigHome`/`SessionHome` (host) | `Warm ∧ LocalDir` → restore/harvest; `Gateway` → skip |
| 7 | Placement | intent → plan → registry → executor trait object | Hand (`HandRequirement`→`ConnectionPlan`→`PlacementEntry`→`RemoteToolExecutor`) and ACP sandbox (`SandboxTier`→`LaunchSource`→channel sources) are two instances of one pattern | placement NEVER enters the snapshot; kernel sees only the trait object |
| 8 | Capability reconciliation | supply ∩ demand, fail-closed | five instances of one shape: envelope@publish, `available_modes`@handshake, `SecurityScheme`@discovery, catalog-fingerprint@dispatch, cli-match@open | never a silent downgrade |
| 9 | Published language & identity | `ResolvedSpec`/snapshot + `CatalogFingerprint` | the only cross-plane language; fingerprint = identity; append-only `skip_serializing_if` serde discipline | why `kind` stays a derived view until a fingerprint-versioning migration |

Residual, deliberately-unfinished unifications: family 1 still has three concrete
shapes (config `ModelSelection`, resolver `AxisBinding`, vault `CredentialBinding`)
— this ADR forbids a fourth and notes convergence for a later resolver-boundary
revision; family 5 collapses only the one intra-context overlap.

## Decision

### D1 — One kind-discriminated authoring aggregate

`AgentConfig` becomes identity + an `AgentKind` discriminant + shared axes whose
names are identical across kinds:

```rust
pub struct AgentConfig {
    // identity (common; excluded from the fingerprint)
    pub id: String, pub name: Option<String>, pub description: Option<String>,
    pub metadata: BTreeMap<String, String>,

    // executor discriminant (axis ①) — replaces backend_ref smuggling
    pub kind: AgentKind,

    // GENUINELY-SHARED axes (≥2 kinds consume; capability-gated). All types here
    // are runtime-contract-level or config-store-local — see the dependency-
    // direction constraint (friction #11): this aggregate must NOT gain a
    // dependency on the vault / catalog / resolver crates.
    pub model: ModelSelection,               // Native + ACP
    pub model_fallbacks: Vec<ModelBinding>,  // was `model_candidates`
    // deliberately ABSENT: any credential field. Agent ⊥ credential is a
    // preserved ADR-0043 invariant; vendor auto-pooling is a resolver default (D5).
    pub instructions: String,                // Native + ACP (A2A gates out)
    pub skill_ids: Vec<String>,              // capability; ref SkillSpec.id (do NOT re-type)
    pub mcp_server_ids: Vec<McpServerRef>,   // capability; ref McpServerDef.id (do NOT re-type)
    // KIND-SPECIFIC axes live INSIDE the kind's spec, not here (D3a).
}

#[non_exhaustive]
pub enum AgentKind { Native(NativeSpec), Acp(AcpSpec), A2a(A2aSpec) }
```

`model_candidates` is renamed **`model_fallbacks`**: "candidates" reads as a pool
and collides with the credential pool; "fallbacks" states "the retry sequence
after the primary model fails". The two pools no longer share a word.

**Shared-axis rule.** A field is a top-level shared axis iff *more than one kind
consumes it*; otherwise it lives in that kind's spec. This is why `skills`/`mcp`
are shared (Native runs them in-process; ACP delivers them into the CLI; both
consume the *capability*), but `plugins`/`tools`/`context_policy` are **Native-only
and live in `NativeSpec`** (D3a): an external ACP CLI is opaque — it never runs
our plugin pipeline or reads our tool catalog — and an A2A remote owns everything.
Forcing those into a "consistent" top-level would be false consistency, and it
would make a meaningless `tools` field spellable on an ACP agent. Keeping them in
`NativeSpec` makes the illegal state *unrepresentable* (stronger than a runtime
gate).

`skills`/`mcp` are **not** re-typed here (reuse map, friction #11): the typed
forms exist downstream — `SkillSpec` (`awaken-ext-skills`) and `McpServerDef`
(`awaken-config-resolver`, itself carrying a `CredentialBinding` so MCP credential
pooling is free). The aggregate references them by id; the resolver owns the join.
The aggregate carries no credential field at all — agent ⊥ credential is
preserved, not eroded (D5).

### D2 — Publish-time capability checks: pure `compile` rules, no envelope type

A kind advertises what it *can* honor; the agent declares what it *wants*;
`compile` fails closed when demand exceeds supply.

The envelope only needs to gate the **genuinely-shared-but-optional** axes
(`skills`, `mcp` — top-level, but an A2A remote honors neither). Native-only axes
(`tools`/`plugins`/`context_policy`/`hand`) need no envelope bit: they live in
`NativeSpec` and are structurally unrepresentable on other kinds (D3a).

Two booleans do not deserve a struct. Following the house style of pure decision
functions (ADR-0056 "keep decisions pure"), the gate is fail-closed
`CompileError` variants raised by `compile` itself:

```rust
// CompileError — ADD:
//   UnsupportedCapability { axis: &'static str /* "skills" | "mcp" */, kind: &'static str }
// compile(): A2a ∧ (!skill_ids.is_empty() ∨ !mcp_server_ids.is_empty()) → error
```

Supply-vs-demand keeps its shape (family 8) but is expressed as a pure check, not
a capability type — a silent runtime no-op (e.g. `skills` on an A2A agent) becomes
a publish-time error with zero new types. Structural placement (D3a) covers the rest.

### D3 — Per-kind specs; the model dialect is NOT stored on the ACP spec

#### D3a — `NativeSpec` holds every Native-only axis (plugins, tools, context, …)

The native in-process loop is the only kind that runs our plugin pipeline, reads
our tool catalog, applies our context-trimming policy, and drives our multiagent
orchestration. Those axes therefore live *inside* `NativeSpec`, not at the top
level:

```rust
pub struct NativeSpec {
    pub hand: HandRequirement,
    pub tools: ToolSelection,           // tool_ids / patterns / overrides — OUR catalog (ADR-0053)
    pub plugins: PluginSelection,       // plugin_ids / plugin_config — OUR in-process runtime exts
    pub context_policy: ContextPolicy,  // OUR loop's context trim (ACP's peer is AcpSpec.compact_window)
    pub max_steps: usize,               // OUR loop's step budget
    pub multiagent: Option<MultiagentSpec>,
}
pub enum HandRequirement { InProcess, Delegated(HandPlacement) }
pub struct HandPlacement { pub transport: DialAddr,           // reuse ConnectionPlan::DialAddr
    pub required_resource_kinds: Vec<String>, pub min_isolation: Isolation }
```

**Capability vs mechanism.** Top-level `skill_ids`/`mcp_server_ids` are *capabilities*
(what the agent wants); `NativeSpec.plugins` is the *native mechanism* that
provides some of them in-process (`ext-skills` realizes skills, `ext-mcp` realizes
MCP). ACP realizes the same capabilities through a different mechanism (config-home
skill files, `session/new` MCP), so the capability is shared (top-level) while the
plugin mechanism is Native-only (`NativeSpec`) — no duplication between the two.

#### D3b — `AcpSpec` / `A2aSpec`

```rust
pub struct AcpSpec {
    pub cli: AcpCliId,                       // axis ① — which CLI. NOT a dialect.
                                             // DERIVED from backend_ref "acp:<cli>";
                                             // never stored twice (one source of truth).
    pub session_mode: Option<SessionModeId>, // validated vs advertised at handshake
    pub session_reuse: SessionReuse,         // Warm | ForcedCold
    pub compact_window: Option<u64>,         // tokens
    pub extra_args: Vec<String>,
    pub config_home: ConfigHomePolicy,
    // deliberately ABSENT: provisioning — how a CLI installs is a property of the
    // CLI row / deployment, not of an agent (D7).
}
// AcpSpec has NO `dialect` field. The dialect is derived from `model`
// (Offering.dialect). Putting a dialect on AcpSpec was the modelling error.
// AcpSpec IS the plugin_config["acp"] codec — ONE serde type in the shared
// contract crate, consumed by both the authoring view and the ACP executor;
// a separate `AcpSettings` struct would duplicate it.

pub struct A2aSpec {
    pub endpoint: String,                    // was smuggled in backend_ref
    pub target_agent: Option<String>,        // delegate path vs peer path
    pub io_modes: IoModes,
}
```

There is **no auth type on the agent — and no new auth table anywhere**: the A2A
`AgentConfig` *is* the remote agent's definition, and `CredentialSource` *is
already* the "counterparty → credential" table. `provider_id` generalizes from
"model vendor" to **counterparty — whoever this credential authenticates to**: a
catalog vendor slug (`"anthropic"`) on the model path, an endpoint origin
(`"https://agents.example.com"`) on the A2A path. Zero schema change — the field
is already an opaque `Option<String>` and `can_consume` already compares strings.

A remote agent then authenticates **exactly like a native agent — the same
declare → select → materialize lifecycle, only the inject exit differs**
(family 4): the transport seam extracts the run's endpoint origin, derives the
counterparty pool (the same `derive_vendor_pool`, keyed by origin), materializes,
and injects into `HttpTransport` (`with_credential`/`with_refresher`) instead of
into a model client or an env var. The injection *shape* (bearer vs api-key
header name) comes from the discovered `AgentCard.security_schemes` — discovery
data, not config; default Bearer. No counterparty-tagged credential = anonymous
(today's behavior); a card that demands auth with none fails closed.

### D4 — Two independent publication checks with one owner per fact

The resolver enforces dialect compatibility (① ↔ ②) and vendor match (③ ↔ model)
separately.

**Placement correction (verified against the crate graph):** `awaken-run-executor-acp`
does **not** depend on `awaken-model-catalog`, and must not — the executor is a
neutral leaf that consumes already-resolved strings (friction #11: dependency
direction). The executor therefore stores stable protocol tokens rather than
importing the control-plane `ApiDialect` type. `AcpCli::model_api_dialects` is the
one declarative capability row; publication compares
`Offering.dialect.as_str()` to it. This keeps dependency direction intact without
introducing a second resolver-side CLI table.

```rust
// awaken-run-executor-acp — declarative AcpCli rows:
//   claude:["anthropic_messages"], codex:["open_ai_chat"], ...
// Publication check A (dialect compatibility, ① ↔ ②):
//   Offering.dialect must be present in the selected row's capability list.
// Check B (vendor match, ③ ↔ model):
//   CredentialSource.provider_id == Offering.provider_id (existing can_consume).
//   Independent of dialect, so minimax × anthropic-messages × claude is legal.
```

### D5 — Vendor auto-pool: upgrade the resolver's DEFAULT derivation; the agent stays credential-free

The repo already has everything: `CredentialSource` rows tagged `provider_id`,
pool selection (`SelectionPolicy`/`eligible_order`/`AvailabilityLedger`),
`can_consume` gating — and one deliberate invariant: **the agent never names its
credential** (ADR-0043 keeps the planes disjoint). This ADR *preserves* that
invariant. `AgentConfig` gains **no credential field**, and no `CredentialBinding`
variant is added either. The entire feature is a behavior upgrade of the single
default-derivation site:

```rust
// config_executor.rs today:  model → offering.provider → FIRST Active can_consume → Exact
// after:                      model → offering.provider → derive_vendor_pool → pool selection
fn derive_vendor_pool(ws: &str, vendor: &str, sources: &[CredentialSource])
    -> Vec<CredentialSource>    // workspace × provider_id × Active;
                                // selection reuses the existing pool machinery verbatim
```

Dropping N same-vendor keys into a workspace makes any agent on that vendor
spread across them — no pool authoring, no wire change, no new aggregate (a
derived read-model). Explicit control keeps its existing home:
`InferenceProfile.credential_binding` (`Exact`/`OneOfCredentialPool`) overrides
the default per model, and `McpServerDef` bindings cover MCP through the same
enum; remote endpoints need no row at all — counterparty-tagged credentials plus
the same derivation (see D3). If a profile ever needs to say "auto"
*explicitly*, an `AutoPool` variant on `CredentialBinding` is the deferred
follow-up — not needed while absence-of-profile already means auto. Default
policy stays `FirstHealthy` (behavior-identical to today's "first Active" with
one key; rotation appears only when several exist). The inline selection at
`config_executor.rs:69` becomes one resolver-owned pipeline:
`credential_candidates` selects for secret-free publication and
`resolve_credential` materializes that same ordering where permitted.

### D6 — Config authors intent; the runtime snapshot stays secret-free and kind-unaware

`compile` projects `kind → backend_ref` (`Native→"genai"`, `Acp→"acp:<cli>"`,
`A2a→"a2a:<endpoint>"` — auth is origin-matched by the host's transport seam at
run, never carried) and `AcpSpec/skills/mcp → ResolvedSpec` (typed in,
serialized to the unchanged `plugin_config` wire through one
`AcpSpec::{from,into}_plugin_config` ACL). `HandRequirement` and
`CredentialRequirement` are **side-channel** inputs to the host resolver/placement
and never enter the snapshot. The runtime still routes via `Backend::from_ref`
and never learns `AgentKind`. Secrets exist only from `materialize` to injection
(model client / ACP `model_delivery.key` env last / A2A transport), per ADR-0043.

### D7 — CLI acquisition: startup resolves one executable route; runs never install

ACP is the only kind that may need to acquire a third-party executable (Native
is compiled in; A2A runs remotely), so acquisition remains ACP-specific. The
canonical `AcpCli` row declares one `AcpAcquisition`: either a direct executable
and arguments, or an exact npm package plus its stable bin name. There is no
second provisioning catalog and no per-Agent installation policy.

For a local installation, product startup discovers the external CLI first and
then installs a required pinned wrapper once below the Awaken data directory.
It stores the canonical absolute wrapper argv in the ephemeral
`AcpWorkerProfile`, which already owns the exact routes that Worker advertises.
The immutable catalog continues to own model, MCP, environment, credential and
probe policy; the resolved argv is only acquisition evidence.

Every run launches that resolved executable directly. It never invokes
`npx -y`, never fetches a package, and has no `Installing` lifecycle state. A
restart reuses the existing wrapper without requiring npm or network access.
Acquisition failure projects `ProbeFailed` diagnostics and prevents route and
WorkerLocal-binding registration. It never falls back to a floating package or
another adapter. Container/remote Workers must bake or otherwise provide their
declared executable before registration; local host acquisition is not an
isolation mechanism.

**"Provisioning" is THREE concerns, and a sandbox sharpens the middle one.**
(1) *Acquire* = fetch a **third-party** binary (ACP-only, at product startup).
(2) *Materialize-into-isolation* = make the needed executable + runtime deps
**present inside the sandbox rootfs** — applies to **anything run under a sandbox
tier, regardless of whose binary**. (3) *Bring-up* = spawn + get a duplex channel.

Per kind, on the host: Native needs none (compiled in); A2A needs none (remote is
already up, just dial); ACP needs acquire + bring-up; Hand needs only bring-up
(it is *our* `Role::Hand` binary — nothing to acquire, `bootstrap` does not apply).

**But under a sandbox tier, concern (2) is unavoidable even for our own binary.**
A fresh bwrap namespace / container rootfs contains nothing unless bound-in or
baked-in: the bwrap tier read-only-binds the host userland (`/usr,/bin,/lib,…`,
`namespace.rs:111`) so host-installed interpreters/CLIs appear; the container tier presents only what the image baked. So a
**sandboxed** ACP CLI must be host-installed+bound or image-baked, and a
**sandboxed** Hand — our binary — must likewise be bound/baked. Materialization is
realized by the tier (bind vs image), not by config.

**Fail-closed rule: acquisition never occurs in a run or sandbox.** A
deny-egress sandbox launches under `--unshare-net` (`namespace.rs:105`), so its
executable must already be materialized by the selected tier. A missing
executable fails before launch with a clear diagnostic; no runtime network
fallback exists. Materialization and egress enforcement live in the sandbox
tier/channel source, shared by every sandboxed kind.

Bring-up's **substrate** is already partly shared and should be more so. The
duplex-byte-channel abstraction is **one trait today** —
`awaken-agent-channel::AgentChannel` (`AsyncRead+AsyncWrite+Unpin+Send`) — produced
and consumed by BOTH the ACP path (`AgentSession.channel: Box<dyn AgentChannel>`)
and the Hand/connection-plan path (`ChannelFactory::connect -> Box<dyn
AgentChannel>`). Environment realization is now shared by Native and ACP through
the Session-owned `SessionEnvironment`; ACP's `BoundLocalChannelSource` only
projects a launch into that already-realized environment. Hand's
`ChannelFactory` still does a raw dial with no isolation. The family-7 reuse is
to route a *sandboxed* Hand spawn through the same Session-environment port. But
the **protocol
ports** stay three: `AgentChannelSource` (ACP session) / `ChannelFactory`
(hand-wire channel) / `Transport` (A2A HTTP) return three different things for
three call sites; one unifying trait would erase the type distinction that makes
miswiring uncompilable — the same over-unification rejected for the late-binding
seams (D9). A unified container **image** (prebake the awaken binary + ACP CLIs +
tool system-packages) is the right *packaging* of "Prebaked everything" for small
deployments; a per-CLI slim image aligns better with capability-aware claim (H)'s
heterogeneous fleet — both are `Prebaked`, differing only in packaging
granularity, a deployment choice this ADR does not fix.

### D8 — GDPR builds on ADR-0050, adding ACP content to Coordinator erasure

The portable ACP session blob is a Coordinator-owned personal-data content
store. The one Coordinator erasure application includes its
`FsSessionBlobStore` adapter when `acp_session_blob_root` is configured; the
adapter persists a subject fence and stable receipt. Runtime transcript capture
is independently narrowed through the Control-owned
`DataSubjectConsentSource`. ACP agents default deny-egress (bwrap
`--unshare-net`) and route model traffic through a region-pinned cloud-managed
gateway (lease token, no raw key in the sandbox). Config text is not personal
data; secrets stay AEAD-sealed and `RedactedString`-guarded.

### D9 — One dispatch flow for all three kinds; differences compress to the execution edge

The durable flow (compile → snapshot → enqueue → claim → resolve → execute →
commit → settle) is kind-agnostic **by construction**: kind is erased to
`backend_ref` at compile and re-materializes only at the execution edge. Phases
0–3 and 6–7 are byte-identical for Native/ACP/A2A (the queue payload is the
secret-free snapshot; the wire has only `enqueue`/`claim`/`renew_lease`/`settle`).
Two rules keep it that way:

1. **Resolve once, inject per kind.** Publication runs the one catalog/credential
   resolution and fingerprints its secret-free result in the executable snapshot.
   The execution edge consumes that fixed access with three injectors:
   model client (Native), `model_delivery` env (ACP, key last), transport header
   (A2A). No kind grows its own resolution path: runtime adapters materialize the
   same published access and may differ only in credential injection/usage.
2. **Capability-aware claim.** A worker declares what it serves — `native` /
   every exact `acp:<cli>` route in its typed `AcpWorkerProfile` / `a2a` — and claim filters on a
   routing key that enqueue stamps from the snapshot's `backend_ref` (no new
   payload, the queue still never parses snapshots). The open-time cli-match
   check stays the fail-closed backstop: the filter is routing, the check is
   enforcement.

Consequently the worker composition root (`awaken_worker::run`) gains the same
ACP wiring the Serve root has (`AcpWorkerProfile` + sandbox tier), and the A2A
transport seam wires identically on both roots. A gateway-only (secretless)
worker serves A2A only when the remote counterparty credential is
gateway-brokered — a local-vault counterparty on a secretless worker fails
closed. The later Session realization contract closes the former resource gap:
Local and Worker runtimes now consume one complete frozen projection containing
the exact Resource revision and resolved manifest; neither reconstructs fields
from mutable Agent configuration or a process-local `prepare_session` path.

A2A is not a fire-and-forget exception to this lifecycle. Its executor implements
the same `RunAttemptExecutor` used by Native and ACP. Immediately after the first
`message:send`, it commits the returned endpoint/task/context identity as Run-scoped
state before polling. A cold replacement therefore reattaches with `tasks/get`, an
awaiting run resumes on the committed context, and durable cancellation addresses
the committed task before the local `Cancelled` commit. Poll and cancel delivery
failures leave dispatch retryable; no recovery path sends a second initial message.
Terminal settlement removes the opaque reference. `agent_run` creates an ordinary
child Run from the target publication and reaches the same A2A
`RunAttemptExecutor` as direct peer admission; no remote directory, transport,
card route or protocol lifecycle is delegation-owned (G39).

### D10 — Lifecycle end: supersede → disable → archive → erase

The flow so far stops at "run"; an agent's end of life becomes explicit, each
state fail-closed at its own boundary:

```rust
pub enum AgentLifecycle {
    Published,   // admits runs; re-publish = supersede (content-addressed, free)
    Disabled,    // admission rejects at ingress (never enqueued); in-flight settle
    Archived,    // read-only for audit/replay; the snapshot is RETAINED while any
                 // committed run still references its catalog_fingerprint
}
// Legal transitions are methods; an illegal transition is unrepresentable.
// Erasure (ADR-0050) removes CONTENT (transcripts, session blobs, memory) —
// never the config skeleton: committed history keys on catalog_fingerprint, so
// archived snapshots deliberately outlive subject erasure.
```

- **Supersede** is already free: publications are content-addressed; a changed
  config mints a new fingerprint and old runs keep resolving the old one.
- **Disable** sits at the same altitude as the capability checks: an ingress
  gate, not a queue or worker concern.
- **Credential retirement exists** (`CredentialStatus::{Disabled,Archived}`;
  `derive_vendor_pool` filters `Active`). **Worker retirement exists** (drain:
  stop claiming, finish in-flight). **CLI retirement** = version-pin rollover via
  the `AcpAdapterProfile` override.
- Retention: archived snapshots are garbage-collectable only when no retained
  committed run references their fingerprint — identity integrity outranks
  storage thrift.

**The lifecycle is NOT a trait family.** States are persisted enums with
transition methods (they must serde, cross processes, and match exhaustively);
transitions and admission decisions are pure boundary functions (fail-closed
`Result`); traits are reserved for **substitution ports** — the only places
where multiple implementations stand behind one call site (stores, executors,
channel sources, the erasure fan-out). A `trait Lifecycle` / stage-marker-trait
design would trade exhaustive matching and serializability for dynamic dispatch
that nobody substitutes.

## Conflicts and friction with existing code

This is the load-bearing risk section. Each item names the incumbent constraint,
the collision, and the containment.

1. **Content-addressed fingerprint vs a new `kind` field + rename.**
   `AgentConfig` field order is the canonical serialization order and the
   publication fingerprint; the doc says it "must stay stable". Adding `kind` and
   renaming `model_candidates → model_fallbacks` changes every existing config's
   fingerprint and invalidates stored publications.
   **Containment:** during phase B, do *not* add `kind` as a new stored field —
   derive `AgentKind` from the existing `backend_ref` at load (`From<&Backend>`),
   keep `backend_ref` as the single stored source of truth, and keep
   `model_candidates` as a serde alias for `model_fallbacks`. New typed fields are
   appended `skip_serializing_if`-empty (the existing pattern) so a pre-existing
   config's bytes and fingerprint are unchanged. The `kind` enum is an *authoring
   view*, not a wire field, until a deliberate fingerprint-versioning migration —
   implemented exactly on the in-repo `ModelSelection` precedent (a semantic enum
   whose custom serde stays byte-identical to the historic flat wire).

2. **`kind` vs `backend_ref` — two sources of truth.**
   Introducing `AgentKind` while the runtime still reads `backend_ref` risks the
   two drifting.
   **Containment:** `backend_ref` remains the *only* persisted/authoritative
   value; `AgentKind` is derived from it and re-projected to it at compile
   (round-trip identity asserted by a test). The runtime is never touched.

3. **`/v1/agents` SDK wire is frozen; `skills`/`mcp_servers`/`multiagent` are
   `Vec<Value>`/`Value` there.** `awaken-protocol-managed::project`
   (`agent_skills → Vec<Value>`, `agent_multiagent → Option<Value>`) and
   `session_repo.mcp_servers: Vec<Value>` are the SDK-consistent projection
   (ADR-0037/0052). Typing these internally collides with the frozen wire.
   **Containment:** the typed `SkillRef`/`McpServerSpec`/`MultiagentSpec` are
   *internal* aggregates; the SDK boundary keeps projecting them to/from `Value`
   (a lossless `TryFrom<Value>`/`Into<Value>` pair). The frozen wire is unchanged;
   only the in-crate representation gains types.

   **Implemented source-of-truth rule:** the frozen route is an HTTP adapter over
   ConfigPlane through `ManagedAgentRepository`; it has no production registry,
   owner side index, or projection fallback. The protocol crate's in-memory
   repository is a test/reference adapter keyed intrinsically by
   `(workspace_id, agent_id)`. Authentication and policy enforcement remain at
   the management edge and are not persisted with the Agent resource.

4. **Default credential selection is "first Active", not a pool.**
   `config_executor.rs` derives `CredentialBinding::Exact` from the workspace's
   *first* Active `can_consume` credential. `AutoPool` changes the default to a
   derived pool.
   **Containment:** default `AutoPool`'s policy is `FirstHealthy`, which is
   behavior-identical to "first Active" when a single credential exists; multiple
   credentials merely gain rotation. No config that has one key per vendor changes
   behavior. `RotateSpread` is opt-in.

5. **`provider_identity_ref` is vestigial (`"default"`).**
   `model_resolver.rs` fills `ModelBinding::new("default", model_id, "default")`,
   so vendor cannot come from `provider_identity_ref`.
   **Containment:** vendor derives from `Offering.provider_id` (already how
   `config_executor` finds the provider), not from `provider_identity_ref`. The
   field stays vestigial here; a later ADR may repurpose it as the auto-pool
   identity/spread key (it is already documented as the cooldown/account-spread
   key), but this ADR does not depend on it.

6. **`AcpCli` is `Copy` with `&'static str` fields; a DB `AcpAdapterProfile` cannot
   be `&'static`.** The catalog is compile-time static.
   **Containment (reuse, not duplicate):** do **not** introduce a parallel owned
   `ResolvedAcpCli` struct. Change `AcpCli`'s fields to `Cow<'static, str>` /
   `Cow<'static, [_]>` so the *same* type serves the `&'static` built-in default
   (borrowed, still effectively zero-cost) and the DB override (owned) — dropping
   `Copy` for `Clone`. Projection functions are unchanged (they already read the
   fields). The dialect-compat table lives in the resolver, not on this row (D4).
   This avoids an eighth "same data, two shapes" pair in the ACP layer.

7. **`A2aRunExecutor` is "config-free by design"; its `TransportFactory` is keyed
   only by URL** (`Arc<dyn Fn(&str) -> Arc<dyn Transport>>`), so per-agent auth
   cannot thread through it, and `over_http()` wires no credential.
   **Containment:** keep the executor reading `endpoint` from the resolved
   `backend_ref` (unchanged, still config-free). Move per-agent auth to the host:
   the `TransportFactory` is replaced by an activation-aware seam
   (`Fn(&RunActivation) -> Arc<dyn Transport>`) that derives the counterparty
   credential pool from the run's endpoint origin, materializes, and builds an
   authed
   `HttpTransport` (`with_credential`/`with_refresher`, both existing). The
   executor's config-free property is preserved; the auth binding is a host
   composition concern, matching how the delegate path already bakes auth into a
   host-constructed transport (ADR-0048/0049).

8. **Hand is host-side placement (ADR-0046), keyed by `root_agent_id`, wired once
   at startup — the resolved spec has no hand field.** Elevating
   `HandRequirement` into config needs a config→placement bridge that does not
   exist and must not put placement into the snapshot.
   **Containment:** add a deploy-time translator that reads published
   `AgentConfig.kind = Native{ hand: Delegated(..) }` and emits
   `PlacementEntry::for_agent(id, RemoteToolExecutor over ConnectionPlan)` into
   the existing `ConfigToolExecutorProvider`. `ResolvedSpec` gains no field; the
   kernel still sees only a `ToolExecutor` trait object. Friction is the new
   bridge, not a runtime change; phase F is gated on ADR-0046 placement staying
   the sole runtime seam.

9. **Db-less worker catalog-fingerprint parity (open gap).** More config-plane
   resolution (dialect check, vendor pool) widens the surface a db-less worker
   cannot reproduce; published-agent full ACP runs already fail closed there.
   **Containment:** all new resolution (checks A/B, auto-pool) runs in the
   config-plane resolver and is captured in the snapshot's fingerprint inputs
   *before* dispatch; the db-less worker path continues to fail closed rather than
   re-resolve, unchanged by this ADR. Not regressed, not fixed here.

10. **`session_mode` is a free `Option<String>` validated at handshake.** Typing
    it `SessionModeId` is a newtype only.
    **Containment:** `SessionModeId(String)` preserves the free-string wire and
    the fail-closed `available_modes` check; no behavior change.

11. **`awaken-agent-config` is the pure upstream authoring aggregate — it depends
    on neither the vault, the model catalog, nor the resolver** (only
    `awaken-runtime-contract`). Putting vault/catalog/resolver types on
    `AgentConfig` (`CredentialBinding`, `ApiDialect`, `McpServerDef`) would invert
    the dependency graph.
    **Containment:** `AgentConfig` references those planes only by opaque id/ref
    (`skill_ids`, `mcp_server_ids`, `credential: Option<CredentialOverride>`); the
    downstream resolver owns every typed join (it already holds `InferenceProfile`,
    `McpServerDef`, `CredentialBinding`, `ApiDialect`). The executor catalog owns
    only stable dialect capability tokens on each `AcpCli`; it does not import
    `awaken-model-catalog`. Publication performs the typed Offering-to-token join
    once, so there is no resolver-side duplicate table. This is the single most
    important constraint on the shape: it is why D5 extends `CredentialBinding`
    in the vault rather than adding a credential enum to agent-config.

## Reuse of existing types (normative — do not re-invent)

Every concept this ADR needs already has a home. The implementation MUST reuse the
right column; introducing a parallel type is a defect, not a phase.

| Concept in this ADR | Reuse THIS existing type | Location | Do NOT create |
|---|---|---|---|
| Credential selection incl. auto-pool | existing `CredentialBinding` + upgraded default derivation (`derive_vendor_pool`) | `awaken-credential-vault` / `awaken-config-resolver` | ~~`CredentialRequirement`~~, ~~`CredentialOverride`~~, any agent-side credential field |
| Credential pool selection/rotation | `CredentialPool` / `SelectionPolicy` / `AvailabilityLedger` / `eligible_order` | `awaken-credential-vault` | a second selector |
| Resolve a binding → credential | `config-resolver::credential_candidates` → `resolve_credential` materialization | `awaken-config-resolver` | inline Exact/default/Pool `.find` selectors in publication or execution |
| Authored MCP server | `McpServerDef` (already carries `CredentialBinding`) | `awaken-config-resolver` | ~~`McpServerSpec`~~ (would be the 8th MCP type) |
| Skill definition | `SkillSpec` | `awaken-ext-skills` | ~~`SkillRef`~~ struct (reference `SkillSpec.id`) |
| "select one / spread across pool" | `AxisBinding<T>` (`Pin`/`Pool`) | `awaken-config-resolver` | a fourth model/credential-axis shape |
| Model API dialect | `ApiDialect` | `awaken-model-catalog` | any `Dialect` type on config |
| ACP CLI catalog row | `AcpCli` (fields → `Cow<'static, str>`) | `awaken-run-executor-acp` | ~~`ResolvedAcpCli`~~ |
| Remote-endpoint auth | `CredentialSource.provider_id` generalized to *counterparty* (vendor slug or endpoint origin) + the SAME default pool derivation; injection shape from discovered `SecurityScheme`; serves BOTH A2A paths (peer executor + `with_remote_a2a` delegation registry) | `awaken-credential-vault` + host seam | ~~`RemoteAuthBinding`~~, ~~`RemoteAgentDef`~~, ~~`RemoteAuthRule`~~ (each was a second counterparty→credential table), hand-built delegation transports |
| Hand/channel auth resolution | implement `connection_plan::CredentialResolver` as an adapter over `resolve_credential` (binding → materialize → `AppliedAuth`) | host over `awaken-connection-plan` | a second credential-resolution source of truth |
| ACP launch-source selection | publicize the existing `LaunchSource{Fixed,Projected}` | `awaken-runtime-host::sandbox_source` | ~~`AcpLaunchSpec`~~ mirror enum |
| Model materialization for ACP | existing `LaunchResolver` backed by snapshot `ResolvedModelCandidate` + shared `PinnedCredentialMaterializer`; environment only advertises worker CLI capability | host/provisioning seam | a second model-materialization truth or ambient provider fallback |
| ACP settings codec | ONE `AcpSpec` serde type (= the `plugin_config["acp"]` codec) shared by authoring + executor | shared contract crate | a separate `AcpSettings` duplicating it |
| CLI acquisition | existing `AcpAcquisition` on the `AcpCli` row + startup-resolved argv on `AcpWorkerProfile` | executor catalog + product composition root | runtime install stage; per-agent provisioning; second adapter catalog |
| Hand transport | `ConnectionPlan` / `DialAddr` | `awaken-connection-plan` | ~~`HandTransport`~~ (alias `DialAddr`) |
| Hand placement entry | `PlacementEntry` / `ConfigToolExecutorProvider` | `awaken-coordinator::placement` | a second placement registry |
| GDPR erasure / consent | ADR-0050 eraser fan-out + `consent_ceiling` | `awaken-data-subject-application` | any new erasure path |

External-dependency auth thus has exactly **one declare-side table**
(`CredentialSource`, keyed by counterparty) plus two optional refinements:
**agent-referenced** when the dependency has its own shared identity
(MCP → `McpServerDef`), **resolver-matched** when a model needs an explicit
override (`InferenceProfile`). A2A needs neither — counterparty tagging + the
default derivation suffice; the agent stays credential-unaware everywhere.
`AgentCard.security_schemes` validates demand; nothing constructs credentials
outside `resolve_credential`.

## Redundancy to consolidate (pre-existing + plan-induced)

Two buckets. The plan-induced ones are prevented by the reuse map above; the
pre-existing ones are opportunistic cleanups this work should fold in where a
phase already touches the file.

Pre-existing (clean up as adjacent phases land):
- **Seven MCP-server representations** — `awaken-ext-mcp::McpServer`,
  `acp_cli::McpServerConfig`, `protocol-managed types/session::McpServer`,
  `protocol-managed state/types::McpServerBinding`, `protocol-acp::SessionMcpServer`,
  `config-resolver::McpServerDef`, `awaken-ext-mcp` id types. Target: `McpServerDef`
  is the config-plane source of truth; the others are legitimate plane-local
  projections (runtime/acp/managed-wire) — but the two protocol-managed shapes
  (`McpServer` + `McpServerBinding`) overlap and should collapse to one.
- **Two credential-selection paths** — `config_executor.rs:69` inline `.find`
  vs `resolve_credential`. D5 collapses them to one resolver-owned candidate
  operation; secret-free publication and permitted management materialization
  consume the same ordering instead of selecting independently.
- **Three model-axis representations** — `ModelSelection`+`model_fallbacks`
  (config), `AxisBinding<T>` (resolver), `model_binding`+`model_candidates`
  (ResolvedSpec). Do not add a fourth; longer-term, express the config-side pair
  through `AxisBinding` semantics when the resolver boundary is next revised (out
  of scope here, noted so phase A does not entrench a fourth).

Plan-induced (must not happen — enforced by the reuse map): `CredentialRequirement`,
`CredentialOverride`, `RemoteAuthBinding`, `RemoteAgentDef`/`RemoteAgentTarget`/
`RemoteAuthRule` (each a second counterparty→credential table or a second record
for the same agent), `CapabilityEnvelope`, `McpServerSpec`,
`SkillRef` struct, `ResolvedAcpCli`, `HandTransport`, `AcpLaunchSpec` (mirror of
`LaunchSource`), an `AcpSettings` struct separate from `AcpSpec` (one codec type),
per-agent provisioning, and storing `AcpSpec.cli` separately from `backend_ref`
(a second source for axis ①).

## Rollout — intent-named phases, retire-with-introduce

Every phase carries five fields: **Intent** (one sentence, one intent per
phase), **Adds**, **Retires** (a hard deliverable, not a follow-up), **Guard**
(the fail-closed check that makes regression impossible), **Done when**
(behavioral, includes "old path greps to zero"). Commits are tagged with the
phase name in scope, e.g. `feat(config): typed-axes — …` (thin ≤4-line messages,
lefthook-enforced).

**0 `pin-invariants`** — *Complete. Every "byte-identical /
behavior-identical" claim is an executable test.*
Adds: characterization tests — publication-fingerprint byte-stability over a
corpus of existing configs; `plugin_config` wire round-trip; single-credential
selection behavior identity; the `backend_ref` routing table.
Retires: nothing.
Guard: this suite IS the guard every later phase leans on.
Done when: the suite runs in lefthook/CI and fails on any wire or fingerprint
drift.

**A `typed-axes`** — *Complete. One codec owns ACP settings; skills/MCP are id
references; zero historical wire change.*
Adds: `AcpSpec::{from,into}_plugin_config` codec; `skill_ids`/`mcp_server_ids`
refs; `tools`/`plugins` grouping (serde-flatten views); `model_fallbacks` alias;
`project_mcp_for` shared by all channel sources.
Retires: free fns `compact_window()`/`mcp_servers_of()` (`subprocess.rs:274,284`);
hardcoded `mcp_session_servers: Vec::new()` in both sandbox/container sources
(`sandbox_source.rs:356`); the overlapping protocol-managed `McpServer` +
`McpServerBinding` pair collapses to one.
Guard: wire round-trip test — old `plugin_config` JSON decodes bit-identically.
Done when: sandboxed CLI receives MCP in k3d e2e; no direct `.get("acp")` outside
the codec.

**B `kind-lens`** — *Complete. The executor axis is a typed authoring view; the
wire does not change.*
Adds: `AgentKind` on the `ModelSelection` serde-lens precedent; publish-time
capability `CompileError` checks (D2).
Retires: nothing (pure view) — but bans a stored `kind` field.
Guard: `kind ⇌ backend_ref` round-trip property test; fingerprint byte-stability
test over pre-existing configs.
Done when: an A2A config declaring `skills` fails publish with
`UnsupportedCapability`.

**C `serve-selected-cli`** — *Complete. The config plane's CLI choice takes
effect in production, on server and worker roots.*
Principle (user-affirmed): the ACP executor runs both **directly in the runtime
(unsandboxed)** and **inside a sandbox**, and is **unaware of which** — it drives
whatever `AgentChannelSource` it is handed. Selecting the environment is a
worker + provisioning concern at the composition root, never the executor's. So
`SandboxTier` gains a `Local` (unsandboxed subprocess) member beside
`Namespace`/`Docker`/`Podman`/`K8s`; the Runtime Host realizes the selected
`SessionEnvironment` once and binds ACP to it. The executor construction is
identical across all tiers.
Adds: one typed `AcpWorkerProfile` shared by Worker capability advertisement and
Host launch routing (`AWAKEN_ACP_CLIS`, with singular `AWAKEN_ACP_CLI` as the
compatibility input); `SandboxTier::Local`; `LaunchSource{Fixed,Projected}`
publicized as the factory input for all tiers.
Retires: the per-attempt `SandboxChannelSource`,
`SessionRuntimeProjectionSource`, and `build_acp_channel_source` parallel path;
fixed argv remains an explicit dev/test `LaunchSource` only.
Guard: `LaunchSource::resolve` cli-match fail-closed (exists); tier-matrix unit
tests.
Done when: one Worker can exact-route `acp:codex` and `acp:claude`; an
unadvertised CLI fails closed, and bare `acp` is accepted only when the profile
has an unambiguous configured default.

**D `one-resolution-path`** — *Complete (2026-07-28).* Exactly one counterparty resolution; dialect
checked; vendor keys auto-pool.*
Adds: `AcpCli::model_api_dialects` + publication check A; `derive_vendor_pool`
default + check B; `LaunchResolver` as adapter over `resolve_inference`.
Retires: the inline `.find(first Active)` at `config_executor.rs:69` and
`EnvLaunchResolver` entirely; database-less workers fail closed instead of acquiring
ambient provider configuration.
Guard: single-key behavior-identity test (`FirstHealthy` ≡ today); unsupported
dialect is a publication candidate rejection.
Completion evidence is maintained in ADR-0069: the existing pool owns policy
ordering, resolver owns default/Exact/Pool selection, publication freezes the
chosen revision, and all three former Server selectors are absent.

**E `authed-remote`** — *Complete (2026-07-28). A remote agent authenticates through the same claim-frozen credential path; only the injection exit differs.*
Adds: counterparty generalization of `provider_id` (docs + origin tags, zero schema); activation-aware transport seam; `connection_plan::CredentialResolver` as adapter over `resolve_credential`.
Retires: hand-built transports at `with_remote_a2a` call sites; the URL-only `TransportFactory` as production wiring (`over_http()` stays test-only); `NoAuth` as the silent default where a card demands auth.
Guard: card-demands-auth-with-no-credential fails closed; anonymous endpoints byte-identical to today.
Done when: one origin-tagged credential authenticates both A2A paths (peer + delegation) in tests with zero per-path wiring.
Completion evidence (2026-07-28): publication pins `ModelProvisioning::Remote`,
optional `CredentialAccess` and card fingerprint. The canonical card projection,
resolver, claim compiler and pinned materializer fail closed on auth, revision,
holder, receipt or fingerprint drift. The former `RemoteAgent*`, `with_remote_agent` and delegation card route are absent. Direct and durable
children select the one `A2aRunExecutor` from the published backend and always
carry placement; the authenticated delegation loopback plus peer-resolution,
placement and materializer tests cover the same production chain.
**F `declared-hand`** — *Complete (2026-07-29). Hand is declared intent;
placement stays host-side; the snapshot stays placement-free.*
Adds: `AgentConfig.hand: Option<String>` as a logical deployment id; typed
`hand_connections: BTreeMap<String, ConnectionPlan>` in deployment config; and a
declared-source mode on the existing `ConfigToolExecutorProvider`.
Retires: production per-Agent `PlacementEntry` assembly. Static entries remain
the deterministic scenario/test adapter, not a second product registry.

Static structure: agent-config validates the logical id; ConfigService's one
installed-publication projection retains it beside (never inside) the executable
snapshot; the CLI composition root dials each deployment-owned `ConnectionPlan`
once; the existing Server placement provider joins the Agent id to that ready
executor. Runtime, the kernel and tool executors depend only on the existing
`ToolExecutorProvider` port and know nothing about discovery, topology or dialing.

Dynamic behavior: startup validates all plans and connects Unix/TCP Hands or
fails before serving; publish/warm-load atomically installs snapshot plus logical
declaration; activation asks the provider for the current declaration; absent
declaration preserves local execution, a known declaration selects its already
connected remote executor, and an unknown/ambiguous declaration fails closed.
Changing `hand` republishes placement intent without changing snapshot bytes or
fingerprint. Connection retry remains a deployment restart/reconciliation
concern—never attempt-time discovery.

Guard evidence: cause/effect decision tables cover placement-free fingerprinting,
empty-id validation, installed-projection ambiguity/uninstall, typed topology
acceptance/rejection, and provider outcomes for absent/known/unknown/source-error
declarations.

**G1 `startup-acquisition`** — *Complete.* Exact wrapper metadata lives on the
canonical `AcpCli` row; the local product composition root installs it below the
Awaken data directory and writes only the resolved argv into the Worker profile.
Retires: runtime `npx -y`, `is_dynamic_install`, bootstrap command mirrors and
the `Installing` lifecycle state.
Guard: missing acquisition evidence prevents route registration; restart reuses
the installed path without network; container Workers remain pre-provisioned.

**G2 `erasable-acp-content`** — *Complete. Portable ACP session content joins the
ADR-0050 Coordinator erasure application.*
Adds: the subject-keyed `FsSessionBlobStore` adapter with a durable erasure fence
and stable receipt; transcript capture behind `DataSubjectConsentSource`.
Retires: thread/adapter-only durable blob keys and order-dependent eraser
registration — closes the GDPR gap without a parallel compatibility path.
Guard: erasure e2e — after Art.17 erase, ACP session blobs for the subject are
gone, retries return the same receipt, and late harvest cannot recreate them.
Done when: the Coordinator erasure target includes every durable subject-keyed
ACP content adapter.

**H `capability-claim`** — *Complete (2026-07-29). The queue routes runs only
to workers that can serve them.*
Adds: worker `serves` declaration (`native`/`acp:<cli>`/`a2a`); enqueue stamps
the routing key from the snapshot's `backend_ref`; claim filters on it.
Retires: nothing — open-time cli-match stays as enforcement (filter is routing).
Guard: a mixed queue never strands a run on an incapable worker in the k3d
harness.
Done when: codex runs drain only to codex workers under load.

Completion evidence: `execution_capability(backend_ref)` is the single
backend-to-capability projection (`native-runtime`, exact `acp:<cli>`, generic
`a2a-runtime`). `remote_worker_placement` stamps it for the primary and every
fallback candidate; `derive_standard_manifest` advertises the same exact ACP ids
from `AcpWorkerProfile`; and every Memory/SQLite/Postgres atomic claim invokes
the shared `WorkerSnapshot::accepts`/`can_claim` compatibility kernel. The
mixed-queue conformance test interleaves twelve Codex/Claude Runs and proves each
Worker drains only its six exact routes on both local dispatch backends.

**I `retire-and-archive`** — *Complete (2026-07-29).* An agent's end of life is
explicit and fail-closed (D10).
Adds: `AgentLifecycle{Published,Disabled,Archived}` + ingress admission gate;
fingerprint-referenced retention rule; erasure stays content-only (ADR-0050).
Retires: nothing (closes the lifecycle gap).
Guard: a run for a Disabled agent is rejected at ingress, never enqueued;
erasure e2e leaves archived snapshots resolvable for replay.
Done when: disable → in-flight settle → archive → erase runs green in e2e, and
replaying an old run against an archived fingerprint still resolves.

Completion evidence: `AgentConfig::lifecycle()` is the single typed
`Published | Disabled | Archived` projection. The existing durable Agent
repository owns both transitions; disable and archive uninstall only the
current executable pointer, while the installed/durable publication catalogs
retain exact fingerprint lookup. Session creation and event admission consult
that same unavailable projection. A run that crossed admission before disable
can settle, while neither a new Session nor a new event can start a new run
afterward. The cause/effect lifecycle test covers transition idempotency,
mutation/publication fences and fingerprint retention; the Managed HTTP/official
SDK E2E covers disable, existing/new Session admission, archive and wire state.
ADR-0069's product Art.17 fan-out E2E proves the subsequent content-only erase
without deleting replay metadata.

Dependencies: A → D (codec); B independent; C independent; E after D
(derivation); F after B (kind); G1/G2/H independent after C; I after B (the
lifecycle gate reuses the publish/admission boundary).

Commitment and order: **0, A, B, C, D, E, F, G1, G2, H and I are complete** — C
first because it is pure wiring with immediate production value (the two
already-merged per-agent-CLI capabilities go live). F was promoted and completed
when declared deployment placement became a product requirement.
**G1/G2/H were trigger-gated** and all have since completed. I was promoted and completed
when Agent decommission became a product requirement. A fired trigger promotes
a remaining phase into the committed queue —
plan-level YAGNI: the design cost is paid (this ADR), the build cost waits for
evidence.

### Cleanup discipline (how redundant/deprecated code actually leaves)

1. **Retire-with-introduce.** The phase that lands a unified path deletes the
   path it replaces *in the same phase*. Two live implementations of one concept
   never cross a phase boundary; "Retires" is acceptance criteria, not backlog.
2. **Delete, don't deprecate, internally.** Pre-1.0 and no external consumers:
   internal redundancy is removed outright. `#[deprecated]` is reserved for
   `public-api/` surfaces only.
3. **The ban list is executable.** Primary: `clippy.toml`
   `disallowed-types`/`disallowed-methods` for the banned identifiers
   (`CredentialRequirement`, `CredentialOverride`, `RemoteAuthBinding`,
   `RemoteAgentDef`, `RemoteAgentTarget`, `RemoteAuthRule`, `CapabilityEnvelope`,
   `McpServerSpec`, `ResolvedAcpCli`, `HandTransport`, `AcpLaunchSpec`,
   `AcpSettings`). Backstop: a lefthook grep over `crates/` for the same names
   (catches definitions clippy can't see). The reuse map stops relying on
   reviewer memory.
4. **"Done" includes absence.** Each phase's verification greps the retired
   symbols to zero, in addition to the new path's tests passing.
5. **Traceability.** Commit scopes carry the phase name, so `git log --grep`
   reconstructs each phase's introduce/retire pair.

### Hardening via Rust mechanisms — make the invariants compile

| Invariant | Mechanism | Violation becomes |
|---|---|---|
| Dependency direction (agent-config ⊥ vault/resolver; application ⊥ infrastructure; runtime ⊥ control) | the Cargo graph plus declared context/layer metadata | boundary-check/compile error |
| Kind-specific axes unrepresentable on other kinds (D3a) | data lives on enum variants, not on flag-guarded shared fields | compile error |
| One source of truth for axis ① | the stored `backend_ref` is private to agent-config; the `AgentKind` lens is the only constructor/reader (parse, don't validate); routers `match Backend` with **no `_` arm**, so a new variant forces every router | compile error |
| No secret in any snapshot / queue payload | `RedactedString` implements no `Serialize`; `ResolvedSpec`'s field list *is* the whitelist | compile error |
| Banned parallel types | `clippy.toml` `disallowed-types` (primary) + lefthook grep (backstop) | lint / commit error |
| Env reads only at composition roots | clippy `disallowed-methods` on `std::env::var`, allowed only under `crates/bin/*` | lint error |
| Container tier without its compiled feature | existing `cfg` fail-closed constructors (kept) | startup error |
| Data-dependent invariants (capability, dialect, counterparty, cli-match, lifecycle) | **cannot compile-time-check data** — fail-closed errors at the earliest boundary: publish (`CompileError`) > resolve (`ResolveError`) > open (`OpenError`) > ingress (lifecycle) | publish/run rejection |
| Public enums that will grow (`Backend`, `AgentLifecycle`) | `#[non_exhaustive]` + `#[must_use]` on decision fns | downstream compile nudge |

The last rows state the honest limit: the type system hardens *structure*;
boundaries harden *content*. Configuration is data, so its invariants get the
earliest fail-closed boundary, never a silent default.

### Assembly — three composition roots, one rule

One role axis (`Role::{Serve,Worker,Hand}`, `awaken-cli/main.rs:44`), three
roots: `awaken all-in-one` (combined process assembly), `awaken_worker::run`,
`awaken-scenario-host` (e2e). The rule: **roots read env and build providers;
everything else receives ports** — enforced by the clippy env rule above. Every
seam is a named `SharedHost` builder method (`with_executor_provider`,
`with_acp`/`with_projected_acp`, `with_config_service`, `with_worker_upstream`,
`with_tool_executor_provider`, `customize_host` last-mile). A phase that adds a
seam adds it to **all three roots** or documents the abstention (e.g. a
gateway-only worker abstains from vault seams). Guard: one composition test per
root boots it on in-memory stores and asserts its seam set — assembly drift
fails CI, not production.

## Consequences

- Positive: kind/dialect/vendor are three closed-vocabulary axes an illegal
  combination cannot even be spelled; skill/MCP support is publish-time checked;
  ACP modes are typed and DB-storable; A2A gains auth; Hand becomes declarative;
  a vendor's keys auto-pool by default; the runtime is untouched and still
  secret-free.
- Negative / cost: the genuinely net-new surfaces are small and bounded — a
  config→placement bridge (F), the
  `AcpSpec` plugin-config codec and a lossless typed↔`Value` boundary for the
  frozen SDK wire (A); phase E adds no new type at all (a semantic generalization
  of `provider_id` plus one host seam). Everything else is a *reuse or extension* of an existing type (D5 upgrades
  one derivation function; D6 uses `Cow` on `AcpCli`; skills/mcp reference
  `SkillSpec`/`McpServerDef`), and the work retires more redundancy than it adds
  (one credential path, one less overlapping managed MCP shape). The
  `kind`-as-derived-view constraint (B) defers a true stored discriminant to a
  future fingerprint-versioning migration.

## Alternatives considered

- **Store `kind` as a first-class wire field now.** Rejected for this ADR: it
  breaks every fingerprint and forces a publication migration for zero runtime
  gain, since the runtime routes on `backend_ref` regardless. Deferred to an
  explicit versioning migration.
- **Adopt awaken-next's `AdapterProfile`-as-store-of-record wholesale.** Rejected
  as premature (YAGNI): the compile-time catalog covers the built-in CLIs; the DB
  override (`AcpAdapterProfile`) is added only behind a concrete "add a CLI
  without a release" need.
- **Explicit operator-authored credential pools only (no auto-pool).** Rejected:
  the requirement is that a vendor's same-type keys pool *automatically*; a
  derived read-model over the existing selection machinery meets it with no new
  persistence.

## Amendment (2026-07-23): typed Agent publication is the implemented authoring boundary

This amendment replaces the earlier proposed `McpServerDef` /
`AgentMcpConfig` authoring split and the raw `Vec<Value>` passthrough described
above. It does not implement the still-proposed Native/ACP/A2A discriminated
aggregate. The implemented bounded slice keeps the existing `AgentConfig`
aggregate and removes duplicate authoring truth.

### Static structure

```text
Managed/Admin HTTP anti-corruption adapters
  -> Workspace-scoped AgentConfig revision
       - ModelSelection + ordered ModelBinding fallbacks
       - AgentMcpServerBinding { name, url, CredentialRef? }
       - skill_ids
       - MultiagentConfig { agent_ids }
  -> ConfigService publication
       - ModelPublicationResolver
       - CredentialReferenceValidator
       - compile_resolved
  -> StoredPublication / ExecutableAgentSnapshot
       - ResolvedModelCandidate[]
       - normalized AgentBindings
  -> AgentConfigSource
  -> persisted Session runtime envelope
  -> SessionRuntimeSlot
  -> execution adapters
```

There is one mutable behavioral aggregate: the Workspace-scoped
`AgentConfig` revision. The external SDK unions are accepted only by the HTTP
adapter and normalized on entry. `MultiagentConfig` stores one coordinator
roster; the legacy `workers` spelling is read compatibility and immediately
serializes back as `{type:"coordinator", agents:[...]}`. Skills store ids, not
SDK object unions. MCP stores one typed endpoint plus an optional exact,
secret-free `CredentialRef { id, revision }`.

`AgentBindings` is not another authoring aggregate. It is the normalized,
fingerprinted execution value compiled into the immutable snapshot. Likewise,
`AgentConfigView` is a query/projection port over the installed publication,
not a repository. The removed `McpServerDef`, `AgentMcpConfig`,
`ResolvedMcpServerView`, their repositories, and their HTTP routes must not be
reintroduced.

`AgentInputConfig` remains separate by design. It owns Agent-to-resource
identity defaults whose Memory/Repository configuration versions are selected
at Session creation under ADR-0063. It has a different lifecycle and invariant
from Agent behavior, so merging it into `AgentConfig` would couple publication
to mutable resource configuration and would not remove duplicate truth.

The Runtime Host holds one private `SessionRuntimeSlot` per Session for
process-local realization: workspace, frozen manifest, environment, resources,
Memory, Skills, MCP, model routing, sandbox and egress. The durable Session
record and its frozen manifests remain authoritative. The slot is replaceable
cache/realization state and cannot be addressed by HTTP authoring APIs.

### Dynamic behavior

```text
author
  -> normalize public wire
  -> CAS one AgentConfig revision
  -> publish in trusted Workspace
       -> resolve model-only selection to one exact catalog binding
       -> validate every exact MCP credential owner/status/revision
       -> compile normalized AgentBindings
       -> persist/install immutable snapshot
  -> create Session
       -> read installed snapshot, never the current draft
       -> apply explicit Session-local MCP overrides
       -> freeze effective MCP/Skill/delegate values and resource manifest
       -> materialize exact credential revision during preparation
       -> install one SessionRuntimeSlot
  -> execute
```

Draft edits cannot affect an existing publication. Republishing affects only
later Sessions; an existing Session keeps its persisted effective values across
retry and restart. Session-inline MCP is a distinct, explicit Session override,
not a second Agent authoring repository. It is frozen into the Session envelope
before Runtime preparation.

Missing or ambiguous model selection, duplicate/self/empty delegation entries,
missing credential validation wiring, cross-Workspace credentials, inactive
credentials, revision mismatch, or Runtime owner/revision mismatch fails closed.
Runtime may materialize the published credential but may not choose another
credential or consult current Agent configuration.

Legacy schema cleanup is a scoped migration: historical migration files remain
immutable, while the latest migration drops the retired MCP aggregate tables.
Legacy import is an explicit startup action and never runs in an HTTP handler,
Runtime constructor, repository query, or normal `open()` read path.

### Simple design and DDD assessment

- **Single source of truth:** one editable `AgentConfig`; immutable publication,
  Session envelope and runtime slot are successive cross-context values, not
  parallel mutable aggregates.
- **Make invalid states hard to express:** delegation and MCP bindings are typed;
  exact credential revisions and complete model bindings are validated before
  publication.
- **Fewest elements:** the old MCP aggregate, repository, DTO, route and generated
  contract family is deleted instead of deprecated.
- **Dependency inversion:** configuration application services depend on model
  and credential validation ports; control-plane adapters implement them.
  Runtime receives immutable values and narrow materialization ports, never
  configuration repositories.
- **Clear orchestration/runtime boundary:** ConfigService coordinates authoring
  and publication; the Managed Session application freezes one executable
  Session; Runtime validates, realizes and executes it. Runtime does not edit,
  publish or re-resolve Agent configuration.

## Amendment (2026-07-27): backend-owned local ACP login is trusted host execution

The amendment's complete static structure, dynamic lifecycle, L1–L6 failure
table, trusted-local security consequences and implementation evidence are
owned by
[ADR-0069](0069-acp-capability-configuration-lifecycle.md). ADR-0057 retains
ownership of the Agent aggregate, mutually exclusive `Provider | BackendOwned`
provisioning union and secret-free publication boundary. Keeping the detailed
ACP lifecycle only in ADR-0069 prevents two normative descriptions of
discovery, observation, placement and launch revalidation from drifting.
