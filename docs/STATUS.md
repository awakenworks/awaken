# Design Status

This repository is configured as the `awaken-runtime` design corpus and
guardrail home. Architecture execution is tracked separately from the retained
document-readiness and role-catalog navigation below. Design decisions remain in
their owning ADRs and code.

Document-readiness values:

| Value | Meaning |
|---|---|
| Ready | Names context, model, port, owner, guardrail, enforcer, and first slice |
| Runtime-owned | Belongs to the runtime package or its public runtime contract |
| Boundary-only | Documents an adjacent system only to keep it out of runtime core |
| Product-owned | Belongs in a downstream product or server/protocol project, not runtime core |
| Retrieval-ready | Correct as a downstream index, but not a source of truth |
| Exploratory | Not ready to drive implementation |

## Documentation Model (ADR-0001)

Per [ADR-0001](adr/0001-documentation-model-and-vocabulary-alignment.md), this
repository is migrating to a decision-first model: ADRs plus a guardrail index
with concrete enforcers ([INVARIANTS.md](INVARIANTS.md)), with one internally
consistent vocabulary. This affects the documentation layers differently:

- **Retained — `docs/wiki/*` (LLM retrieval index).** The wiki is a distinct,
  downstream layer for locating the authoritative owner quickly — not a competing
  source of truth. It is kept, with tightened discipline: every fact is
  **link-only** (one sentence plus an owner link) and never copies schemas,
  guardrail ranges, state machines, or tables. The drift this caused (e.g. the
  stale `G1 through G16`) came from breaking that rule, not from the layer; the
  `check-wiki-no-invariant-copy` hook enforces it.
- **Trimmed — process/meta tables.** The readiness, document-class, and
  role-catalog-coverage tables below, and `requirements-coverage.md` as a
  standalone matrix, are process/status surface. They locate the owning document
  for a role catalog; they do not own role semantics or state machines. They are
  slated for simplification once their load-bearing facts fold into ADRs or the
  guardrail index, and are retained for navigation during migration only.

## Architecture Execution

Current main source `a4fc7d6a3140` (2026-11-06) retains five normally
committed CI/fixture changes after `8b908ffec6f4`; the rebased combination
still needs its own source-bound qualification. Private checkpoint
`31645771afd4` earned `verified/20261106-0823` for the original complete
Linux Rust group: 10,561 passed and 129 skipped, both workspace Clippy
graphs and doctests passed; the earlier main checkpoint keeps its own receipt.
This is the single progress record for the crate-boundary program; decisions
remain in their ADRs and code, and executed results remain in the
[qualification record](design/test-validation.md#integrated-source-qualification-record).
Plan v5 is in force; the v4 slices below identify their existing owners.
The automatic Resource receipt and exact concurrent-convergence repairs are
normally committed and fast-forwarded. The fixed-source ACP continuation `8594ef7915b0`, its conditional child
observation follow-up `689d604978c8` and guard consolidation `5758bd9fee47`
are privately received with owning qualification. Their four replayed patches
and all incoming main afterimages are preserved; the new source, full E2E,
formal and downstream results retain their own subjects.

- **Delivered checkpoints.** V4, the E2E continuation, SDK baseline qualifier
  repair, G, nanos-image proof and BackgroundTask consolidation have normal
  commit and protected FF receipts; no push is claimed. Their distinct source
  subjects and executed scopes remain in the [qualification record](design/test-validation.md#integrated-source-qualification-record).
- **Tags.** `verified/20261106-0823` peels to private `31645771afd4` and records
  the original complete Linux Rust group only. Earlier tags retain their own
  source and scope; the rebased combination, whole214, SDK release, full13
  and product evidence stay separate.
- **Source versus delivery.** G's runtime `2903a249449a` and normal delivery
  `a4402c05ca68`, nanos follow-up `fcf046d5150e` and BackgroundTask follow-up
  `2d195c4174b7` each retain their own evidence. Remaining formal, SDK release,
  coverage and live acceptance stay separate in the [qualification record](design/test-validation.md#integrated-source-qualification-record).
- **Current continuation.** Typed IAM failure handling, Org-authorized principal
  selection, the stronger poison guard, exact-identity fixture cleanup and the
  canonical lazy-role builder are normally committed and fast-forwarded.
  The shared extraction fixture now reads and writes the canonical frame;
  its original crash, durable and stage-recovery cases passed before normal
  delivery and again on rebased `f5c7d3f91a07` before protected FF.
- **Capture boundary.** Native construction now freezes the actual file-trace
  selection once and shares the existing [D7 ceiling](adr/0050-telemetry-content-capture-consent-and-gdpr-erasure.md#d7-erasure--retention-are-subject-keyed-store-capabilities-partitioned-by-org)
  across Control, Session and Run; downstream callers retain their own cohort
  verification. Structured spans and frozen public declarations keep their
  stated observation scope. Controlled neutral-request regressions now cover
  preloaded Skill input and ordinary/self child isolation; actual live child
  model input and raw output remain unobserved.
- **Received continuation.** Shared trait projection, the required libtest
  observer, canonical Skill policy, published Memory proposal selection and
  the two Resource receipt/convergence fixes are normally committed and in
  the reviewed main checkpoint. The original unmodified Namespace case passed
  live Resource changes, cross-Run reuse, hard-crash adoption and release;
  this single case is not whole214 or physical-cluster qualification.
- **Source diagnostics.** On `8b908ffec6f4`,
  `python3 scripts/ci/principle_status.py --guards` reports 0 lock-poison panics,
  33 `Weak`, 56 optional borrowed roles, one wire error rendering and 30 contract
  crates; its governance view reports 195 Python scripts / 80,774 lines and
  10 baseline files / 753 entries. These hits retain the classifications and
  removal conditions in [injection and typed errors](design/injection-and-error-cleanup.md);
  they are not a count of unexplained violations.
- **Boundaries.** Build/test receipts are source-bound; Windows/MSVC remains
  unverified. Size and method counts are diagnostics ([ADR-0091](adr/0091-architecture-rules.md));
  W23 directory relocation is deferred and is not a completion gate.

| Area | Existing owner / mechanism | Open evidence |
|---|---|---|
| P0 Linear main | One integration writer and protected fast-forward, no implicit push ([ADR-0091](adr/0091-architecture-rules.md)). | Preserve source identity across each normal commit and FF. |
| P1–P3 Realization | One `RealizationError` disposition and lifecycle-held peer access ([ADR-0089](adr/0089-session-realization-single-owner.md)). | Requalify the final combined source; old results do not qualify later changes. |
| P4 Executor | Caller-role components, Coordinator closure guards and one existing execution pipeline ([executor status](design/runtime-interface-boundaries.md#executor-status)). | G is delivered with source-bound Rust/PG and normal-hook evidence in the [record](design/test-validation.md#integrated-source-qualification-record); live acceptance remains open. |
| P5 Session state | The aggregate decides Environment transitions; the realization driver orchestrates recovery ([ADR-0090](adr/0090-session-state-model.md)). | V4 and G are delivered; their distinct source subjects remain in the [record](design/test-validation.md#integrated-source-qualification-record). |
| P6 Sandbox | One provider selected as provisioning/Session role handles ([provider boundary](design/sandbox-provider-contract.md#session-role-handles-2026-10-31)). | Final Linux/container/distributed acceptance and Windows evidence remain separate. |
| P7 Application / protocol | Use-case components share `SessionMutations` CAS; protocols translate ([application status](design/session-aggregate-and-dispatch-capabilities.md#application-status-2026-10-31)). | Complete-admission and V4 caller migrations are delivered; downstream and product acceptance remain separate. |
| Contract convergence | Canonical owners and imports follow [ADR-0090](adr/0090-session-state-model.md) and [ADR-0091](adr/0091-architecture-rules.md); distinct Dream/provider/transport contracts remain distinct. | V4 and G are delivered with source-bound check results; downstream callers still need their own cohort qualification. |
| Formal | The proof owner records tools, domains and resource limits ([formal trust](design/formal-proof-trust.md)). | The original observation mutant repair is delivered; fresh complete formal qualification remains open in the [record](design/test-validation.md#integrated-source-qualification-record). |
| Integrated qualification | Original Rust/PG/API entrypoints, normal hooks and protected FF ([record](design/test-validation.md#integrated-source-qualification-record)). | Whole-SHA formal/release/live evidence remains open; G's delivered gates do not establish full13 or coverage. |
| Downstream | Flow still resolves Native `81aef316f004`, while Cloud's normal private pin unit resolves qualified `615c885bd4e7`; IAM remains `07af02a7`. | Flow's browser environment unit `8c754bb50f3d` is normally committed, with complete browser/whole qualification open. Final producer pins, constructor migration, tooling, fingerprint and images must bind one qualified cohort. |

### Plan v5

| Item | Scope | Status |
|---|---|---|
| V0 | Source-bound Rust, PG and cold recovery qualification | The original complete Linux Rust group passed on private `31645771afd4`: 10,561 passed and 129 skipped, including the original default-stack two-Run/Vault MCP case and all four Resource races. Tag `verified/20261106-0823` binds that checkpoint; the rebased main combination needs its own complete run. |
| V0b/V0c | Container-instance identity and same-Run/Session recovery | Host cold recovery passed; joined physical restart/Run recovery remains unverified. The separate real Skill journey failed before its restart stage. |
| V1 | Prefer compiler/Cargo controls; consolidate actual duplicate guards | Normal `2df68c09a677` consolidates three trait walkers into the existing lexer/span owner, while the canonical libtest observer refuses empty-child evidence. Privately received `5758bd9fee47` consolidates the overlapping Run-admission predicate and route delimiter parser after original self-tests, full boundary/docs checks and normal hooks passed. |
| V2 | Preserve lock-poisoning policy and typed failures | IAM `07af02a7` (0.2) and Native typed failures, atomic publication and Org credential authority are delivered. The reviewed main has zero production poison-panic hits and passed both original workspace Clippy graphs; Windows remains separate. |
| V3 | Complete construction roles and typed boundary errors | The actual file-sink ceiling is frozen once and shared across Control, Session and Run; `b876b2f19acf` also retains the selected lifecycle through the mounted Scenario family and joins it before runtime shutdown. Downstream caller migrations retain their own combination qualification. |
| V4 | Canonical bounded-context contracts and `RunIngress` naming | Delivered; the historical Baseline interpreter stays with its existing type owner without a new upward edge. Later G results retain their own source subject. |
| V5 | Keep one short status record and linked design owners | This is the single progress owner; executed details stay in the existing qualification/M5 owners without a second status ledger. |
| V6 | Every formal gate proof finishes within host resources | The `3eeb9532` full run completed Kani/vacuity and TLAPS before the observation-mutant failure repaired in `395303e5`; the later full attempt returned resource 75. Fresh complete qualification and the existing rerun markers remain open. |
| V7 | Upgrade the real Cloud/Flow composition cohort | Flow and Cloud retain their incoming main changes in clean rebased candidate trees, still resolving Native `81aef316f004` and `615c885bd4e7` respectively. Final qualified Native pins, the Flow constructor migration, actual fingerprint/BOM and both complete validation graphs remain open. |
| V8 | Linux, k3d and distributed product acceptance on one final BOM | Whole214 has a retained delegation counterexample on `31645771afd4`; required Provider/Skill journeys, full13, distributed acceptance, final BOM and coverage remain open. The approved minimal historical reference still needs a complete comparison; controlled Provider checks and earlier partial profiles keep their narrower scopes. |
| V9 | G private Task control and L SDK SemVer; retire LocalWorker after V8 | G is delivered; all three original SDK package/artifact/compiler-SemVer qualifiers and their outer wrappers passed on `08eef1f2fb88`, with historical and first-release subjects distinguished. Final source and public publication remain separate; LocalWorker retirement follows V8 and requalifies its changed source. |

## Document Readiness

| Document | Status | Notes |
|---|---|---|
| `README.md` | Ready | Defines bounded contexts and development-ready design rule |
| `INVARIANTS.md` | Ready | Guardrails are mechanical and reviewable |
| `requirements-coverage.md` | Ready | Maps all required areas to bounded contexts and packaging rules |
| `design/architecture-overview.md` | Ready | Maps the implemented Control/Coordinator/Resources/Worker boundaries, aggregate ownership, lifecycle processors, and runtime/server seams |
| `design/acp-harness-capability-and-tool-bridge.md` | Runtime-owned | Defines ACP capability selection and projection, the governed Session MCP tool bridge, approval replay safety, verification, and portable working-directory constraints |
| `design/config-to-run-execution-flow.md` | Ready | Owns static Agent/Environment publication, role-scoped database/migration ownership, Deployment/Session creation, and request execution through per-kind materialization, commit, and HTTP/SSE response |
| `design/config-publication-lifecycle.md` | Ready | Owns Control publication and Coordinator executable-registration semantics, idempotency, recovery, and failure rules without a parallel whole-catalog install model |
| `design/protocol-adapter-boundaries.md` | Product-owned | Defines public protocol adapter mapping, conformance, and unsupported management boundaries |
| `design/permission-policy-axis.md` | Runtime-owned | Defines authorization flow, permission decisions, HITL tickets, and audit staging |
| `design/model-provider-backend-binding.md` | Runtime-owned | Defines model-provider/model/model-pool/agent spec graph, selected binding validation, fallback ownership, capability reconciliation, and narrow `ModelProviderSpec` / `AgentSpec` responsibilities |
| `design/commit-fact-projection-taxonomy.md` | Runtime-owned | Separates live stream output, committed truth, replay rows, and public projections |
| `design/builtin-tools-extension-contract.md` | Runtime-owned | Defines `awaken-ext-builtin-tools` package boundaries and toolset contracts |
| `design/key-design-decisions.md` | Ready | Defines runtime-owned implementation decisions, explicit config graph, concrete-tool packaging, admin-tool ownership, neutral naming, publication-role placement, and rejected leaks |
| `design/runtime-behavior.md` | Runtime-owned | Covers run lifecycle, activation/context split, live state apply versus durable commit, state/effects/events, extensions, cancellation, scheduling, eval |
| `design/auxiliary-context-windows.md` | Ready | Defines shared transcript windows, extension ownership, and asynchronous Memory, Compact, and Outcome behavior |
| `design/session-branch-prefixes.md` | Ready | Defines immutable cross-Session transcript prefixes without copied target history or parallel branch state |
| `design/runtime-scenario-validation.md` | Ready | Owns runtime GWT scenario ids, scenario text, executable-test mapping, and scenario test organization |
| `design/test-validation.md` | Ready | Explains test design, risk and execution tiers, affected selection, cache/concurrency limits, coverage interpretation, and environment qualification |
| `design/session-execution-test-acceptance.md` | Design ready; qualification pending | Calibrates the supplied 37 requirements into traceable behavior scenarios and shared gates with source-bound witnesses; no full-group execution acceptance is claimed |
| `design/session-execution-test-acceptance-cases-1.md` | Design ready; qualification pending | Concrete scenarios for the acceptance checklist, split by requirement range; same qualification status as the overview |
| `design/session-execution-test-acceptance-cases-2.md` | Design ready; qualification pending | Concrete scenarios for the acceptance checklist, split by requirement range; same qualification status as the overview |
| `design/session-execution-test-acceptance-cases-3.md` | Design ready; qualification pending | Concrete scenarios for the acceptance checklist, split by requirement range; same qualification status as the overview |
| `design/runtime-interface-boundaries.md` | Runtime-owned | Makes runtime role traits, activation/context/snapshot split, external publication-registration roles, executable snapshot contract, plugin contributions, tool decisions, and simple-design checks explicit |
| `design/neutral-waist.md` | Runtime-owned | Runtime execution ports and extension points |
| `design/tool-and-capability.md` | Runtime-owned | Capability segmentation, neutral ToolExecutor port, builtin-tools placement, unified delegation tool, and permission boundary |
| `design/run-ingress-message-delivery.md` | Boundary-only | Dispatch/server boundary for run ingress, durable delivery, pending input, and message handoff |
| `design/remote-worker-protocol.md` | Boundary-only | Accepted P0/P1/P2 contract for recoverable, database-independent, embeddable remote Workers; PostgreSQL active-active execution is verified without sticky routing, while optional transfer optimizations remain deferred |
| `design/sandbox-provider-contract.md` | Boundary-only | One claimed Sandbox supply port, provider codec/restoration roles, and source migration boundaries; deployment qualification remains separate |
| `design/worker-retirement.md` | Boundary-only | Qualifies existing Actor/source boundaries; deployment and original-mission acceptance remain pending |
| `application-authentication.md` | Product-owned | Integration guidance for service credentials, application tokens, and authenticated frontend transports |
| `design/anthropic-alignment-and-sessions.md` | Product-owned | Downstream product adapter guidance |
| `design/managed-dream.md` | Product-owned | Owns the Managed Dream API and Dream job, frozen JSONL/session evidence, read-only source snapshot, required write-through result MemoryStore, restricted ordinary Agent execution, recovery, and test design |
| `design/managed-deployments.md` | Product-owned | Owns durable Managed Deployment/DeploymentRun scheduling, Workspace scope, Agent version freezing, occurrence claims, lifecycle facts, and Dream scheduler coordination |
| `design/web-ui.md` | Product-owned | Web console blueprint: Oversight two-scope shell over the management plane, session surface, design tokens, contract-first frontend engineering plan |
| `design/credentials-and-vaults.md` | Product-owned | Credential/product concern; runtime sees opaque refs only |
| `design/resources-memory-files-skills.md` | Product-owned | Normative Resources application composition and File/Memory/Repository/Skill lifecycles, including command ownership, config resolution, activation, explicit terminal Repository publication, recovery, and reclamation |
| `design/file-lifecycle.md` | Product-owned | Logical File creation, exact byte resolution, deletion replay and reclamation; delegates common component ownership to the Resources design |
| `design/resources-memory-files-skills-roadmap.md` | Product-owned | Delegates its bounded delivery sequence, exclusions, and guardrail index to the normative Resources design without defining another model |
| `design/runtime-persistence.md` | Runtime-owned | Expands ADR-0039: persistence bounded contexts (agent-truth / dispatch / config / protocol-projection), port surface, `awaken-store-<medium>` backend matrix, atomic staged commit (G13), and fact-authority reads (D4) |
| `design/tool-state-machine.md` | Runtime-owned | Defines the tool call state machine: typed state cells over the untyped command store, the four runtime seams (state materialization, tool gate chain, tool-outcome reaction hook, run-end guard state), reminder emission via the conversation aggregate, capability bounds, and persistence/atomicity/transactionality/restart guarantees |
| `design/plugin-configuration.md` | Runtime-owned | Defines per-plugin configuration: the raw config carrier on the resolved spec, config-aware resolve, validation as a dry run of resolve, schema derived from the config type via schemars, and delivery to the frontend on the capability catalog |
| `design/observability-eval-dataset-boundary.md` | Boundary-only | Separates traces, datasets, eval, and analytics from runtime truth |
| `design/error-taxonomy.md` | Ready | Classifies neutral errors and public adapter error mapping |
| `design/packaging-enforcement-matrix.md` | Ready | Defines package/import/license/vocabulary enforcement expectations |
| `wiki/*` | Retrieval-ready | LLM/wiki facts are synced to source docs and current guardrails |

## Role Catalog Coverage

This table is the machine-checked classification for source documents. It decides
which documents must carry a role/component catalog and which documents are
meta, navigational, delegated, or downstream mapping documents.

The table is not a second source of design truth. A `Role owner` document owns
only the catalog for its stable roles; lifecycle state machines live in the
owning code/Rustdoc when implemented, or in that same owner document before
implementation. Meta, coverage, status, and wiki documents link to those owners.

| Document | Class | Catalog policy | Catalog owner |
|---|---|---|---|
| `README.md` | Meta / introspection | Not required | n/a |
| `STATUS.md` | Meta / introspection | Not required | n/a |
| `INVARIANTS.md` | Guardrail registry | Not required | n/a |
| `requirements-coverage.md` | Coverage map | Not required | n/a |
| `formal-verification-expansion.md` | Coverage report | Not required | n/a |
| `design/formal-proof-trust.md` | Coverage report | Not required | n/a |
| `design/architecture-overview.md` | Navigation / context map | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/config-to-run-execution-flow.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/config-publication-lifecycle.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/protocol-adapter-boundaries.md` | Product/downstream mapping | Not required | n/a |
| `design/permission-policy-axis.md` | Role owner | Required | self |
| `design/model-provider-backend-binding.md` | Role owner | Required | self |
| `design/commit-fact-projection-taxonomy.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/builtin-tools-extension-contract.md` | Delegated boundary narrative | Delegated | [tool-and-capability.md](design/tool-and-capability.md#tool-and-capability-role-catalog) |
| `design/acp-harness-capability-and-tool-bridge.md` | Delegated boundary narrative | Delegated | [tool-and-capability.md](design/tool-and-capability.md#tool-and-capability-role-catalog) |
| `design/key-design-decisions.md` | Decision record | Not required | n/a |
| `design/runtime-behavior.md` | Role owner | Required | self |
| `design/auxiliary-context-windows.md` | Design narrative | Not required | n/a |
| `design/session-branch-prefixes.md` | Product/downstream mapping | Not required | n/a |
| `design/session-aggregate-and-dispatch-capabilities.md` | Role owner | Required | self |
| `design/session-environment-transition.md` | Design narrative | Not required | n/a |
| `design/contract-convergence.md` | Decision record | Not required | n/a |
| `design/runtime-scenario-validation.md` | Coverage map | Not required | n/a |
| `design/test-validation.md` | Meta / introspection | Not required | n/a |
| `design/guard-consolidation.md` | Meta / introspection | Not required | n/a |
| `design/session-execution-test-acceptance.md` | Coverage map | Not required | n/a |
| `design/session-execution-test-acceptance-cases-1.md` | Coverage map | Not required | n/a |
| `design/session-execution-test-acceptance-cases-2.md` | Coverage map | Not required | n/a |
| `design/session-execution-test-acceptance-cases-3.md` | Coverage map | Not required | n/a |
| `design/runtime-interface-boundaries.md` | Role owner | Required | self |
| `design/neutral-waist.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/tool-and-capability.md` | Role owner | Required | self |
| `design/run-ingress-message-delivery.md` | Role owner | Required | self |
| `design/remote-worker-protocol.md` | Role owner | Required | self |
| `design/sandbox-provider-contract.md` | Delegated boundary narrative | Delegated | [Sandbox provider role catalog](adr/0041-sandbox-execution-environment-provider.md#role-catalog) |
| `design/worker-retirement.md` | Delegated recovery qualification | Delegated | [remote Worker component catalog](design/remote-worker-protocol.md#10-remote-worker-component-catalog) |
| `application-authentication.md` | Role owner | Required | self |
| `design/anthropic-alignment-and-sessions.md` | Product/downstream mapping | Not required | n/a |
| `design/managed-dream.md` | Role owner | Required | self |
| `design/managed-deployments.md` | Role owner | Required | self |
| `design/web-ui.md` | Product/downstream mapping | Not required | n/a |
| `design/credentials-and-vaults.md` | Product/downstream mapping | Not required | n/a |
| `design/resources-memory-files-skills.md` | Role owner | Required | self |
| `design/file-lifecycle.md` | Delegated boundary narrative | Delegated | [resources-memory-files-skills.md](design/resources-memory-files-skills.md#resource-input-component-catalog) |
| `design/resources-memory-files-skills-roadmap.md` | Delegated boundary narrative | Delegated | [resources-memory-files-skills.md](design/resources-memory-files-skills.md#resource-input-component-catalog) |
| `design/observability-eval-dataset-boundary.md` | Product/downstream mapping | Not required | n/a |
| `design/prompt-skill-optimization-data-contracts.md` | Proposed implementation contract | Not required | n/a |
| `design/prompt-skill-optimization-state-machine.md` | Proposed lifecycle owner | Not required | n/a |
| `design/error-taxonomy.md` | Decision record | Not required | n/a |
| `design/injection-and-error-cleanup.md` | Decision record | Not required | n/a |
| `design/remote-task-control.md` | Decision record | Not required | n/a |
| `design/lock-poisoning.md` | Decision record | Not required | n/a |
| `design/sdk-versioning.md` | Decision record | Not required | n/a |
| `design/packaging-enforcement-matrix.md` | Meta / introspection | Not required | n/a |
| `design/runtime-persistence.md` | Delegated boundary narrative | Delegated | [runtime-interface-boundaries.md](design/runtime-interface-boundaries.md#role-catalog) |
| `design/tool-state-machine.md` | Role owner | Required | self |
| `design/plugin-configuration.md` | Role owner | Required | self |
| `design/distributed-acp-execution.md` | Design narrative | Not required | n/a |
| `design/brain-hand-coverage.md` | Coverage report | Not required | n/a |
| `design/managed-agents-behavior-conformance.md` | Conformance inventory | Not required | n/a |
| `design/sandbox-scope-closure-qualification.md` | Qualification report | Not required | n/a |
| `adr/0001-documentation-model-and-vocabulary-alignment.md` | Decision record | Not required | n/a |
| `adr/0002-resolver-role-demarcation.md` | Decision record | Not required | n/a |
| `adr/0003-deferred-work-mechanism-selection.md` | Decision record | Not required | n/a |
| `adr/0004-plugin-factory-contributions-and-capability-bound.md` | Decision record | Not required | n/a |
| `adr/0005-run-terminal-state-single-authority.md` | Decision record | Not required | n/a |
| `adr/0006-fact-authority-run-record-is-cache.md` | Decision record | Not required | n/a |
| `adr/0007-runtime-owns-tool-execution.md` | Decision record | Not required | n/a |
| `adr/0008-durable-postgres-commit-backend.md` | Decision record | Not required | n/a |
| `adr/0009-durable-run-ingress-slice.md` | Decision record | Not required | n/a |
| `adr/0010-idempotent-pending-consumption.md` | Decision record | Not required | n/a |
| `adr/0011-autonomous-dispatch-service.md` | Decision record | Not required | n/a |
| `adr/0012-sqlite-and-postgres-store-backends.md` | Decision record | Not required | n/a |
| `adr/0013-pending-lifecycle-and-cross-thread-outbox.md` | Decision record | Not required | n/a |
| `adr/0014-scheduled-delivery.md` | Decision record | Not required | n/a |
| `adr/0015-crash-retry-budget-and-dead-letter.md` | Decision record | Not required | n/a |
| `adr/0016-durable-cancel.md` | Decision record | Not required | n/a |
| `adr/0017-send-message-over-outbox.md` | Decision record | Not required | n/a |
| `adr/0018-priority-dedupe-gc.md` | Decision record | Not required | n/a |
| `adr/0019-distributed-dispatch-and-wake-signal.md` | Decision record | Not required | n/a |
| `adr/0020-scheduled-action.md` | Decision record | Not required | n/a |
| `adr/0021-idle-thread-delivery.md` | Decision record | Not required | n/a |
| `adr/0022-epoch-supersession.md` | Decision record | Not required | n/a |
| `adr/0023-dead-letter-ttl-gc.md` | Decision record | Not required | n/a |
| `adr/0024-daemon-lease-renewal.md` | Decision record | Not required | n/a |
| `adr/0025-dispatch-query-surface.md` | Decision record | Not required | n/a |
| `adr/0026-stop-policy.md` | Decision record | Not required | n/a |
| `adr/0027-scheduled-action-kind-axis.md` | Decision record | Not required | n/a |
| `adr/0028-nats-store-deferral.md` | Decision record | Not required | n/a |
| `adr/0029-built-in-runtime-namespace.md` | Decision record | Not required | n/a |
| `adr/0030-permission-policy-axis.md` | Decision record | Not required | n/a |
| `adr/0031-config-store.md` | Decision record | Not required | n/a |
| `adr/0032-runnable-config.md` | Decision record | Not required | n/a |
| `adr/0033-in-process-run-driver.md` | Decision record | Not required | n/a |
| `adr/0034-runtime-axis-model-and-orthogonality.md` | Decision record | Not required | n/a |
| `adr/0035-environment-provisioning-tools-skills-resources.md` | Decision record | Not required | n/a |
| `adr/0036-skills-as-runtime-extension-single-tool.md` | Decision record | Not required | n/a |
| `adr/0037-managed-capability-advertisement-wire-alignment.md` | Decision record | Not required | n/a |
| `adr/0038-managed-resource-injection-and-store-organization.md` | Decision record | Not required | n/a |
| `adr/0039-runtime-persistence-port-convergence-and-store-naming.md` | Decision record | Not required | n/a |
| `adr/0040-server-durable-ingress-integration.md` | Decision record | Not required | n/a |
| `adr/0041-sandbox-execution-environment-provider.md` | Decision record | Required | self |
| `adr/0042-public-api-tenancy-authz-and-front-door-consistency.md` | Decision record | Not required | n/a |
| `adr/0043-management-plane-config-credential-model-and-runtime-unaware-secret-seam.md` | Decision record | Not required | n/a |
| `adr/0044-remote-hand-tool-executor-over-a-channel.md` | Decision record | Not required | n/a |
| `adr/0045-connection-plan-and-network-topology.md` | Decision record | Not required | n/a |
| `adr/0046-hand-placement-tool-executor-provider.md` | Decision record | Not required | n/a |
| `adr/0047-compaction-as-agent-run-and-the-context-plane-boundary.md` | Decision record | Not required | n/a |
| `adr/0048-iam-host-adoption-org-workspace-path-alignment-and-a2a-carve-out.md` | Decision record | Not required | n/a |
| `adr/0049-a2a-cross-tenant-federation-carve-out.md` | Decision record | Not required | n/a |
| `adr/0050-telemetry-content-capture-consent-and-gdpr-erasure.md` | Decision record | Not required | n/a |
| `adr/0051-tenancy-edge-aspect-one-opaque-scope-id.md` | Decision record | Not required | n/a |
| `adr/0052-management-assistant-ordinary-agent-in-a-reserved-scope.md` | Decision record | Not required | n/a |
| `adr/0053-memory-store-fuse-mount.md` | Decision record | Not required | n/a |
| `adr/0054-safe-loop-boundary-shared-seam-and-pause-as-durable-await.md` | Decision record | Not required | n/a |
| `adr/0055-typed-state-kernel-loop-actions-as-state.md` | Decision record | Not required | n/a |
| `adr/0056-sandbox-reuse-two-orthogonal-volumes.md` | Decision record | Not required | n/a |
| `adr/0057-unified-agent-configuration.md` | Decision record | Not required | n/a |
| `adr/0058-one-neutral-event-vocabulary-message-sourced-classify-routed-two-tier-projection.md` | Decision record | Not required | n/a |
| `adr/0059-neutral-core-and-leaf-evolution.md` | Decision record | Not required | n/a |
| `adr/0060-durable-dispatch-completion-tombstone.md` | Decision record | Not required | n/a |
| `adr/0061-selectable-identity-and-platform-managed-resource-scopes.md` | Decision record | Not required | n/a |
| `adr/0062-published-inference-access-and-runtime-credential-injection.md` | Decision record | Not required | n/a |
| `adr/0063-resource-input-identity-configuration-pinning-and-lifecycle.md` | Decision record | Not required | n/a |
| `adr/0064-runtime-owned-outcome-orchestration.md` | Decision record | Not required | n/a |
| `adr/0065-recoverable-embeddable-remote-worker.md` | Decision record | Not required | n/a |
| `adr/0066-session-service-binding-and-realization.md` | Accepted target with implemented immutable mutation-policy and atomic create-receipt slices; whole guardrail remains target until deployment/E2E evidence is complete | Delegated | [remote Worker component catalog](design/remote-worker-protocol.md#10-remote-worker-component-catalog) |
| `adr/0067-credential-custody-model-exposure-and-secret-delivery.md` | Accepted target landing in verified slices; whole guardrail remains target until implementation/E2E evidence is complete | Delegated | [remote Worker component catalog](design/remote-worker-protocol.md#10-remote-worker-component-catalog) |
| `adr/0068-unified-prompt-and-skill-optimization.md` | Proposed decision record | Not required | n/a |
| `adr/0069-acp-capability-configuration-lifecycle.md` | Proposed decision record | Not required | n/a |
| `adr/0070-local-browser-session-bootstrap.md` | Decision record | Not required | n/a |
| `adr/0071-distributed-service-boundaries-and-executable-agent-registration.md` | Implemented decision record | Not required | n/a |
| `adr/0072-environment-definition-and-execution-boundary.md` | Accepted Environment definition/execution ownership target | Required | self |
| `adr/0073-session-environment-owned-hand-and-worker-capability-placement.md` | Implemented decision record | Not required | n/a |
| `adr/0074-session-environment-suspension-and-checkpointed-continuation.md` | Proposed Session Environment continuation decision | Not required | n/a |
| `adr/0075-unified-managed-session-worker-execution.md` | Implemented Session Worker, complete-profiled-creation, effect-fenced replay, and terminal Repository publication placement decision | Not required | n/a |
| `adr/0076-session-owned-background-tool-execution.md` | Implemented Native BackgroundTask decision | Not required | n/a |
| `adr/0077-iam-directory-product-space-placement.md` | Implemented product-space placement decision | Not required | n/a |
| `adr/0078-terminal-recovery-retains-the-existing-session-authority.md` | Implemented terminal Resource projection, terminal-only release, and bounded same-process Kubernetes physical join | Not required | n/a |
| `adr/0079-one-tool-authorization-vocabulary-and-opaque-backend-configuration.md` | Implemented single authorization vocabulary, retired mode axis, fail-closed baseline, and opaque backend session configuration | Not required | n/a |
| `adr/0080-full-gate-freshness-and-declared-tool-environment.md` | Implemented full-gate record and freshness policy, one declared gate-tool manifest, stage tool/disk prechecks, and the build-space reclamation rules | Not required | n/a |
| `adr/0081-contract-layering-and-ratchet-guardrails.md` | Accepted contract layering; September closure splits Session vocabulary behind its facade, makes Session/Run-ingress/Resource capabilities explicit, types Resource error detail, records canonical conformance owners, and verifies the container projection is not a second Sandbox authority | Not required | n/a |
| `adr/0082-open-specification-closed-substrate.md` | Deferred; keep ADR-0041's current open durable-backend and provider placement until a second closed substrate or a revised self-hosted multi-node floor requires a new product decision | Not required | n/a |
| `adr/0083-managed-cloud-session-durable-acceptance.md` | Implemented local Cloud Managed durable-root acceptance; physical capacity remains a deployment and frozen idle-retention concern | Not required | n/a |
| `adr/0084-shared-worker-transport-assembly.md` | Decision record | Not required | n/a |
| `adr/0085-acp-instructions-and-session-time-context.md` | Decision record | Not required | n/a |
| `adr/0086-runtime-sdk-public-surface.md` | Decision record | Not required | n/a |
| `adr/0087-sqlite-through-sqlx.md` | Decision record | Not required | n/a |
| `adr/0088-bounded-context-map-and-sdk-platform-contract.md` | Decision record | Not required | n/a |
| `adr/0089-session-realization-single-owner.md` | Decision record | Not required | n/a |
| `adr/0090-session-state-model.md` | Decision record | Not required | n/a |
| `adr/0091-architecture-rules.md` | Decision record | Not required | n/a |
| `adr/0092-worker-capacity-and-schedulability.md` | Decision record | Not required | n/a |
| `adr/0093-immutable-application-origin-provenance.md` | Decision record; source carrier/read projection does not qualify product model authorization | Not required | n/a |
| `adr/0094-session-access-and-run-admission-outcomes.md` | Decision record; executed by the Session acceptance ledger tests | Not required | n/a |
| `adr/0095-windows-local-process-tree-ownership.md` | Decision record | Not required | n/a |
| `adr/0097-protected-browser-authentication-boundary.md` | Historical component design; placement superseded by ADR-0099 | Not required | n/a |
| `adr/0098-claimed-credential-delivery-revocation-fence.md` | Decision record; local Windows component witness does not qualify all native/provider facets | Not required | n/a |
| `adr/0099-application-owned-credential-consumption.md` | Generic Native entry/envelope delivery implemented; application browser/Android E2E uses synthetic accounts/sites; production IAM/storage and real provider login remain unqualified | Not required | n/a |
| `adr/0100-credential-handling-and-execution-binding.md` | Unified handling policy and JS/Python SDK; execution admission reuses original Session/Dispatch owners; local qualification and production limits documented | Not required | n/a |
| `adr/0101-acp-application-execution-context.md` | Opted-in ACP/native MCP correlation; original Session authority resolves actual execution; strict request-body closure and Windows external-process E2E | Not required | n/a |
| `adr/0102-session-execution-product-gap-closure.md` | Durable pre-root creation intents; determinate admission refusal; root-level budget-pause interrupt no-op; bounded pool shutdown, LiveInbox and SQLite admission; classified capacity failures | Not required | n/a |
| `adr/0103-session-execution-product-gap-closure-2.md` | Running budget tightening; memory-version redact action; explicit Stop receipts; classified MCP/ACP/Hand-relay faults; cross-base local realization takeover; doctest E-code gate | Not required | n/a |
| `adr/0104-memory-copy-reconciliation-adopts-identical-heads.md` | Memory Copy replay adopts an identical durable head; a lost materialized Copy directory is gone, not empty | Not required | n/a |

## Implementation Context

| Area | Owner | Current guidance |
|---|---|---|
| Runtime protocol/license | Current repository | Protocol/specification, SDK-facing schemas, examples, and conformance tests use Apache-2.0; code packages may carry their own package/file license metadata |
| Runtime core | Current repository | Keep runtime-owned crates/package open and neutral; consume immutable `ExecutableAgentSnapshot`/`RunActivation` data and commit state/effect facts without owning configuration publication or executable registration |
| Dispatch/server | `awaken-dispatch` (host crate) + adjacent server/config package | Application admission selects a private direct (`DirectAttemptDriver`) or durable (`RunDispatch` over `DispatchPool`/`DispatchWorker`) value, and runtime `ActiveAttemptScope` alone owns current-attempt controls and a fresh optional LiveInbox ([run ingress](design/run-ingress-message-delivery.md)). The host depends on runtime and store adapters, never the reverse |
| Config publication and executable availability | Control and Coordinator | Control persists `StoredPublication` and registers its immutable snapshot through `ExecutableAgentRegistrar`; `ExecutableAgentCatalog` is the single Coordinator execution projection ([publication lifecycle](design/config-publication-lifecycle.md)). Registration recovery, active-active refresh and the local/authenticated adapters are in [architecture overview §7](design/architecture-overview.md#7-operational-convergence-and-deployment-topology); Worker receives neither tokens nor authority databases |
| Environment definition and executable availability | Control and Coordinator | `EnvironmentApplication` owns authoring, `EnvironmentRegistrationDelivery` exact-intent delivery, and `EnvironmentExecutionApplication` the sole Coordinator path over `ExecutableEnvironmentCatalog` and `WorkQueue` ([architecture overview](design/architecture-overview.md#11-physical-deployment-and-authority-map)). Archive keeps Control history and withdraws execution availability; Managed owns only HTTP mapping |
| Resource application and lifecycle | Resources, currently co-deployed by Coordinator | One `ResourceAuthorities` value and one `ResourcesApplication` serve artifact harvesting, public File APIs and purge scheduling, while Worker materialization stays per-kind and claim-fenced ([Resources](design/resources-memory-files-skills.md)). No standalone role exists until the split conditions of [architecture overview §7.3](design/architecture-overview.md#73-conditional-resources-process-split) hold |
| Hosted product adapters | Downstream product package or repository | Implement public DTOs/events behind anti-corruption adapters; product hosting vocabulary does not enter neutral runtime/protocol/config code |
| Hosted Management identity | `awaken-cli` + `awaken-control` over the pinned `awaken-iam-host` adapter | Server-mode Cloud identity requires a dedicated projected service-token file, reloads it for each PDP request, requires an explicit user bearer, and fails closed without reusing Flow identity or a cached local login (ADR-0061 amendment) |
| Hosted PostgreSQL schema lifecycle | `awaken database migrate` plus each bounded-context store's canonical scoped bundle | The migration command is the sole DDL writer, and server-mode `connect_existing` verifies exact ledgers without DDL ([migration ownership](design/config-to-run-execution-flow.md)). Control, Coordinator and Resources own unconditional, dense-versioned, checksummed bundles; Worker owns none |
| Unified Managed Session Worker execution | Session + Environment WorkQueue + Runtime Host | [ADR-0075](adr/0075-unified-managed-session-worker-execution.md) is implemented: each profiled create lowers into one finalized revision-1 Session root, and only an `Applied` result continues into realization and dispatch while `Replayed` returns durable truth. [ADR-0083](adr/0083-managed-cloud-session-durable-acceptance.md) and [ADR-0066](adr/0066-session-service-binding-and-realization.md) own durable-root acceptance and mutation authority; `INVARIANTS.md` owns guardrail status |
| Observability/eval | Analytics/DX package or repository | Consume committed facts or normal runtime ports; never become runtime truth |
| E2E coverage | `scripts/ci/e2e-coverage.sh` + `e2e/` | Deterministic served-process API coverage; the gate prints the current numerator and total. The last changed-line audit (2026-07-22) measured 1589/1662 = 95.61% against the >95% threshold ([details](design/test-validation.md#served-process-e2e-coverage)) |
| Distributed role E2E | `deploy/k3d/README.md` + `e2e/k3d/distributed_control_e2e.sh` | ADR-0071's P2-B process-level acceptance runs production Control, replicated Coordinators, two `awaken-worker` processes and four owner databases under fault and load ([scope](design/test-validation.md#distributed-role-e2e-scope)). Worker Pods hold no authority database or seal configuration |
| Session execution placement | Coordinator durable dispatch + registered Worker + Session Environment | [ADR-0073](adr/0073-session-environment-owned-hand-and-worker-capability-placement.md): one capability-bearing Worker executes Native/ACP/outbound-A2A attempts, the Session Environment owns Native Hand and ACP state, and Agent/deployment Hand placement is removed. `SandboxRequirements` is shared by provider and claim admission; `awaken-tool-relay` and `ConnectionPlan` are container Hand transport, not a placement authority |

## Not Ready Without More Detail

The following should not be implemented as broad subsystems from these docs alone:

- a product-first managed crate family as the core architecture;
- a universal execution abstraction that merges application admission, direct
  attempts, durable Dispatch, or live input. The shipped surface is the concrete
  `DirectAttemptDriver` plus `RunDispatch`/`DispatchPool`/`DispatchWorker`
  ([ADR-0009](adr/0009-durable-run-ingress-slice.md)): a durable submit persists an
  accepted run, a worker claims and runs it under a single-owner lease, an expired
  lease is recovered, and an awaiting run resumes through delivered input — all over
  the durable `CommitCoordinator` backend ([ADR-0008](adr/0008-durable-postgres-commit-backend.md)).
  The originally minimal ADR-0009 slice has since gained scheduled wake, lease
  renewal, cross-thread outbox, query/maintenance, supersession, dead-letter, and
  durable completion tombstones through ADR-0011–0027 and ADR-0060. Extend those
  existing authorities and backend-conformance suites; do not create a parallel delivery
  subsystem. The commit contract any backend must satisfy remains fixed by
  [ADR-0006](adr/0006-fact-authority-run-record-is-cache.md);
- product-specific vaults, sessions, and outcome fields inside runtime crates.
- a manager/controller that owns config loading, registry compilation, catalog
  install, live control, and execution as one runtime object;
- a public protocol adapter without conformance fixtures, unsupported-management
  tests, and DTO leak checks;
- an executor/environment channel without correlation, idempotency, and
  indeterminate-result tests.
