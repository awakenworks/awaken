# Formal verification boundaries and architecture roadmap

Formal verification can prove only a precisely stated model or a bounded piece
of implementation. It cannot, by itself, demonstrate every product behavior.
This repository therefore keeps three evidence levels separate:

1. **Direct implementation proof**: Kani, Loom, or checked Rust refinement calls
   the same decision/transition kernel consumed by production code.
2. **Model-linked evidence**: TLC/TLAPS proves a finite protocol model and the
   ledger names related production files. The link is an engineering claim, not
   an automatically proved Rust-to-TLA refinement.
3. **Product evidence**: tests, E2E scenarios, API snapshots, and architecture
   checks demonstrate executable behavior outside the formal model.

`coverage.json` must not relabel levels 2 or 3 as an implementation proof.
`proof-boundaries.json` is the machine-readable inventory of every product
requirement that still has requirement-only production surfaces.

The source denominator is deliberately split. `check_formal_surface.py` scans
Rust production modules, while `check_formal_web_surface.py` scans product-significant
TypeScript/TSX authority, route, storage, state, fetch, and mutation surfaces.
Every Web row is forced to `product_boundary_only`: browser tests and TypeScript
type checking are valuable executable evidence, but are not formal proofs.

## Architecture slices implemented at this checkpoint

- Dream status writes now consume one closed, exhaustively checked lifecycle
  table. Terminal states are absorbing and completion/failure require Running;
  generated-content quality remains an explicitly semantic boundary.
- Resource Catalog publication consumes an exact non-wrapping version kernel;
  lifecycle timestamps and Kubernetes claim-deletion fences have exact checked
  projections. Filesystem, object-store, CSI, and Kubernetes durability remain
  environmental assumptions.
- Durable deployment admission and sandbox-support projection consume bounded
  evidence selectors. Actual PostgreSQL/filesystem persistence, NetworkPolicy,
  registry, builder, container, and kernel enforcement remain external.
- Quiescence, checkpoint, source-disposal, restore, and terminal-cleanup
  receipts require every identity, generation, fence, and settlement axis.
  Database commit atomicity and remote effect execution remain adapter claims.
- Worker observations require an exact verified fact and the complete half-open
  lease interval. This does not prove TLS, DNS, WebPKI, the remote process, or
  the orchestrator.
- Settled Runtime steps and scoped tool-catalog membership now consume closed
  projection kernels: Running cannot cross the settled boundary and only the
  reserved scope can observe admin tools.
- HTTP idempotency keys now pass an exact byte/length admission kernel, keeping
  their original wire identity. Header libraries, proxies, TCP, and TLS remain
  outside the proof.
- The in-memory durable-ingress path consumes one claim-fenced reducer for
  claim, settlement, relinquishment, and cancellation. SQLite/PostgreSQL must
  still be migrated to this reducer and checked as external transaction adapters.
- AG-UI and AI-SDK terminal emission consume one fail-closed absorbing kernel;
  their wire-category mappings are exhaustive. Unbounded tool reconciliation,
  network delivery, client interpretation, and rendering remain outside proof.
- A published Memory plugin remains satisfiable when recall is disabled, but
  the proved selector contributes no recall hook or state authority. Memory
  store durability and extraction effects remain external adapter boundaries.
- Session realization failures now consume a closed seven-variant disposition
  table and one shared Worker-effect kernel: terminal truth is absorbed rather
  than relinquished, while renewal retires it. Database commits, transport,
  cross-process cleanup, and remote execution remain adapter/external claims.
- GenAI transcript replay consumes closed dialect, part-category, row-admission,
  opaque signed-thinking, and reasoning-fold kernels. Anthropic retains the
  ordered text/signature pair while other dialects use normalized reasoning;
  reasoning-only rows are omitted and streaming/non-streaming folds share one
  exact-once table. SDK capture/serialization, signature authenticity,
  unbounded payloads, network and provider/LLM behavior are not proved.

## What remains outside a direct proof

- LLM meaning, quality, usefulness, and truthfulness have no complete decidable
  specification. Structural schemas and finite rubrics can be proved; open
  natural-language correctness cannot.
- TLS/WebPKI, DNS, cryptographic strength, provider revocation, third-party
  peers, database engines, filesystems, kernels, containers, and orchestrators
  are environmental assumptions unless their implementations are brought into
  the verified computing base.
- Eventual delivery, real-clock behavior, scheduler fairness, performance,
  long-run stability, telemetry retention, browser rendering, accessibility,
  and human approval authenticity need explicit assumptions and executable
  evidence rather than blanket proof claims.
- Arbitrary JSON, strings, manifests, plugins, and user-authored workflows are
  unbounded. Kani proofs use finite equivalence classes or bounded collections;
  those bounds must appear in the obligation name or documentation.
- A model-linked TLA+ invariant does not automatically prove the asynchronous
  database/network adapter. A checked trace is stronger, but still covers the
  represented traces rather than every possible implementation execution.

## Architecture changes that expand the provable surface

### Canonical protocol algebra

Define one closed, versioned algebra for lifecycle, failure, permission, and
stream events. Generate ACP/A2A/MCP/HTTP/UI mappings and JSON schemas from it.
Kani then proves every generated mapping total, exact, and non-strengthening;
schema mutation and transport traces cover serialization and I/O.

### Compiled bounded manifests

Compile plugin, skill, tool, resource, and workflow manifests into bounded
closed representations before activation. Unknown identifiers and excessive
depth/size fail closed. Pure selectors and reducers become exhaustively
provable while parsing and storage remain narrow adapters.

### Generated authority and startup tables

Generate role grants, route guards, tool membership, and `StartupRoleWiring`
from closed decision tables. Production builders use typestate such as
`RuntimeBuilder<MissingAuthorization>` so a production runtime cannot exist
without its required authorization gate. Unrestricted construction is isolated
behind an explicit test-only API.

### Shared browser route-scope kernel

Move workspace scope precedence and route parsing out of TypeScript into a
small shared Rust contract compiled to WASM. The browser supplies route and
server context to this pure selector; local/session storage is never an
authority input. Kani can then prove exact precedence and fail-closed handling,
while browser tests retain responsibility for URI APIs, navigation, rendering,
and deployment integration.

### Decide -> CommitPlan -> CommitReceipt

Pure code decides an effect and emits a stable plan identity. An adapter performs
the effect and returns an authenticated receipt. Only an exact, live, fenced
receipt can mutate durable truth. TLA+ checks crash/retry behavior; Kani proves
plan/receipt admission; Rust traces check selected implementation refinements.

### Durable inbox/outbox and event-sourced reducers

Use durable inbox/outbox records for network and side-effect boundaries. Keep
session, run, memory, deployment, and outcome state as deterministic reducers
over immutable events. This makes safety independent of process crashes and
turns database adapters into conformance targets instead of hidden authorities.

### Linear permits and verified typestates

Represent credential materialization, mounts, tool effects, approval, and remote
transport as unforgeable, single-use permits bound to workspace, owner, epoch,
revision, hash, and expiry. Examples include `MaterializationPermit`,
`ContentAddressedSnapshot<Verified>`, `VerifiedTlsChannel`, and
`VerifiedPeerPrincipal`. Adapters issue them only after external validation;
finite kernels prove they cannot be widened or replayed.

### Bounded collections or induction

Where product bounds are real, encode and enforce them at admission. Where data
must remain unbounded, prove one transition preserves an invariant and use
mathematical induction/TLAPS rather than claiming a small Kani vector proves an
unbounded collection.

## Follow-up implementation sequence

1. Canonical protocol algebra and generated adapter tables.
2. Rust/WASM browser route-scope selector plus checked browser traces.
3. Compiled bounded plugin/tool/workflow manifests.
4. Generated role/route/startup authority and production typestate.
5. Complete `CommitPlan`/`CommitReceipt` coverage for remaining effects.
6. Durable inbox/outbox and stable operation identities.
7. Linear credential/resource/approval/transport permits.
8. Event-sourced reducers and backend refinement histories.

Each follow-up is complete only when production consumes the proved kernel, the
harness is named by strict CI, the product requirement links the obligation,
mutation tests can kill a weakened invariant where applicable, and remaining
external assumptions stay explicit.
