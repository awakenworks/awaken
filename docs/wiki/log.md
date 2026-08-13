# Wiki Update Log

## 2026-08-13

- **Consolidation**: The hosted runtime route exporter now derives canonical
  flat and explicit Workspace-prefixed matchers from the same IAM route-policy
  descriptor; Cloud remains a generic matcher compiler with no route list.
- **Update**: Amended ADR-0061 so the existing Management-owned publisher role
  can read executable model supply while remaining unable to administer API
  keys or mutate Provider/model supply; hosted deployments consume the same
  deterministic release profile instead of binding a broader role.
- **Consolidation**: Exposed the Coordinator's narrow PostgreSQL LocalCommit
  factory so product compositions reuse its private authoritative-query policy
  instead of treating a process-local projection as HA truth or copying SQL.
- **Consolidation**: Extended the existing profiled Session command so product
  MCP candidates and published Agent candidates enter the same normalization,
  precedence, finalization, and root-insert path.
- **Correction**: Executable-model readiness now joins Brokered Offerings with
  the existing Cloud model-supply capability instead of demanding a copied
  local provider credential; direct/BYOK readiness remains credential-backed.

## 2026-08-12

- **Update**: Consolidated deterministic Scenario Session recovery onto the
  production Coordinator lifecycle-supervisor registration. Cold protocol reads
  remain effect-free; the supervisor rebuilds frozen Runtime projections and
  resumes durable Memory extraction after process death.
- **Update**: Restored the coordinator-only Scenario's authenticated remote
  Worker path by mounting the canonical private Worker transport on its sole
  listener only when the local pool is disabled; ordinary Scenario surfaces
  remain isolated and no parallel Worker API was added.
- **Update**: Corrected Managed deployment tests to preserve the authoritative
  contracts: unreachable MCP staging is retryable `503`, Host executors cannot
  attest `inference_geo=us`, and `/v1/models` is populated only after Provider
  discovery is frozen into an executable Agent publication.
- **Consolidation**: Accepted ADR-0075 and removed the late Worker-authored
  Session-input path. Complete inputs now freeze before insertion, self-hosted
  Sessions always use the Environment WorkQueue, and Workers realize only
  committed Session truth.

## 2026-08-10

- **Update**: Amended ADR-0070 so local browser sessions reuse IAM's existing
  durable `SessionRepo` and survive Awaken process restart without adding a
  product-owned session or authorization path.

## 2026-07-28

- **Update**: Amended ADR-0061 so embedded IAM and the side-effect-free
  `awaken management iam profile` release projection share one deterministic,
  Management-owned authorization contract.
- **Update**: Extracted the existing `awaken.runtime.resources` policy into the
  same authoritative path and exposed it through
  `awaken management iam profile resources`, so hosted File/Skill authorization
  cannot drift from embedded Management.

## 2026-07-26

- **Update**: Amended ADR-0067 with the neutral Native
  `PlatformProviderAdapter` realization: downstream platforms can bind an exact
  Platform trust-domain holder and claim-fenced receipt without adding cloud,
  gateway, IAM, route, or secret-store vocabulary to Awaken.

## 2026-07-25

- **Correction**: ADR-0066/0067 remain accepted target directions, but their
  feature slices are now gated by a contract-closure Slice 0. The correction
  freezes the ADR-0051 scope envelope, consumed preparation intent,
  the now-superseded late Worker-to-Control Session-input design, canonical Managed
  MCP full replacement, Session realization lease, replace/tombstone root CAS,
  sealed payload resolver, dispatch claim-epoch credential binding, exact OAuth
  refresh/reseal access, and planned-versus-actual realization split. Previous
  entries describing the targets as immediately development-ready are
  superseded.

## 2026-07-24

- **Update**: Accepted ADR-0067 as development-ready for Workload/Worker realization: CredentialAccess separates material source from recipient-bound envelope, Environment/attempt profile requests one exact allowed holder, MCP generation or RunAttempt pins it, and failure never changes holders. Automatic LLM Vault and Platform realization remain separately gated.

- **Update**: Accepted ADR-0066 as development-ready: one root mutation atomically owns immutable baseline, existing Resource state, MCP-only generations, idempotency, and outbox. ADR-0075 later removed its late Worker-input portion; exact generations still stage invisibly, CAS Active, publish, drain, and recover.

- **Update**: Amended ADR-0062 with the hosted configured-upstream composition,
  same-model complete-binding fallback semantics, and canonical Worker realization
  capability vocabulary.
- **Update**: Accepted ADR-0062: inference access is resolved once by Workspace at publication, fingerprinted in the executable snapshot, and only its published credential-injection contract reaches Runtime/Worker.

## 2026-07-20

- **Update**: Indexed ADR-0061 for selectable local identity, centralized authorization scope policy, and platform-managed resource ownership.

## 2026-06-27

- **Update**: Added explicit model-provider/model/model-pool/agent config graph guidance, including spec responsibilities, model binding selection, and model-pool fallback ownership.
- **Update**: Renamed the model-access provider record to `ModelProviderSpec` and clarified that `AgentSpec` must not absorb concrete launch, endpoint, or probe data.
- **Update**: Split immutable `RunActivation`, per-attempt `RuntimeRunContext`, and immutable `ExecutableAgentSnapshot` in runtime boundary and config-to-run guidance.
- **Update**: Clarified that live `StateStore` apply is not durable commit visibility, and reserved durable truth for `ThreadCommit` / `CommitCoordinator`.
- **Update**: Added neutral `ToolExecutor` guidance so host tools, MCP, remote, and client-executed tools remain execution-side adapters.
- **Update**: Clarified runtime axes and recorded that hook/tool/model calls cross execution, staging, commit, and projection before becoming durable truth.
- **Update**: Added facts for config-side publication coordination, registry compilation, and the runtime catalog install boundary.
- **Update**: Added executable snapshot contract facts and linked them to the config-to-run flow.
- **Update**: Added explicit boundary facts for protocol adapters, permission, binding, resources, errors, and package enforcement.
- **Update**: Added the config-to-run execution flow fact page and indexed it from [index.md](index.md).
- **Update**: Recorded the runtime configuration publication axis and neutral naming guardrail in retrieval facts.
- **Update**: Split durable wiki maintenance policy into [maintenance-notes.md](maintenance-notes.md), keeping `log.md` as OKF update history.

## 2026-06-26

- **Update**: Established this wiki as the retrieval index for the runtime design corpus.
- **Update**: Added the runtime protocol/license guardrail and related source-document ownership links.

## 2026-06-25

- **Initialization**: Created the source-document ownership index, fact indexes, agent instructions, and root navigation index.
- **Update**: Added retrieval facts for runtime behavior, runtime interface boundaries, tool and capability policy, deployment, resources, credentials, and product adapter boundaries.
## 2026-07-29 (Hosted Runtime authorization release contract)

- Added one Awaken-owned deterministic Hosted lifecycle authorization profile
  for `run.create/read/resume/cancel`.
- Exported the same contract through
  `awaken management iam profile runtime` so hosts reconcile exact image data
  rather than copying the role/action matrix.
- Validated the release value through the IAM PAP and attached an exact
  Workspace scope rule to every registered lifecycle action.

## 2026-07-29 (Dispatch operational time remains store-owned)

- Amended ADR-0065 and the remote Worker protocol so every newly appended
  dispatch operation carries a store-assigned durable wall-clock timestamp.
- Kept claim/settle operation order as the sole Worker lifecycle authority;
  downstream elapsed-time projections consume the feed and never create a
  second running-state machine.

## 2026-08-11 (Session Running interval remains aggregate-owned)

- Amended ADR-0074 so overlapping activity, idle settlement, and terminal
  intent produce one typed, pricing-neutral Running interval through the
  existing transactional lifecycle outbox.
- Kept checkpoint, restore, queue, drain, and retention work outside that
  interval and prohibited a Billing-specific Session event store.

## 2026-08-11 (Checkpoint custody receives authoritative ownership scope)

- Extended the existing checkpoint byte request with the Session repository's
  Workspace owner scope and immutable creation time so hosted custody can
  resolve tenant DEKs without a Session mirror or guessed identity.
- Kept URLs, credentials, pricing, and storage lifecycle out of the neutral
  contract; the Session aggregate remains the only continuation authority.

## 2026-08-11 (Lifecycle consumers share one outbox delivery path)

- Added one generic ordered lifecycle-delivery composite at the Control
  composition seam so deployment consumers reuse the existing stable fact and
  transactional outbox.
- Every receiver is attempted and any failure keeps the fact retryable; no
  consumer gains a polling loop, Session mutation authority, or parallel ledger.

## 2026-08-12 (One front-door IAM enforcement path)

- Amended ADR-0063 so each public router crosses exactly one canonical IAM edge;
  its route policy selects the management or resource action namespace.
- Removed the overlapping resource middleware path while preserving the trusted
  Workspace-only contract seen by Resource services.

## 2026-08-12 (Realization preserves the activity-owned runtime interval)

- Amended ADR-0074 and invariant G47 so replacement-Worker realization success
  cannot settle an overlapping Running activity.
- Terminal realization failure now closes the same aggregate-owned interval and
  commits the existing lifecycle fact atomically; no second billing event path
  was added.

## 2026-08-12 (Worker shutdown evidence and terminal error truth)

- Connected Kubernetes normal termination to the existing Worker HTTP drain
  seam and kept hard-crash validation at the exact CRI process boundary.
- Kept `EndCause::Error(Failure)` as the committed explanation; the step proof no
  longer demands a duplicate assistant message that could mask that error.

## 2026-08-12 (Domain idempotency remains aggregate-owned)

- Amended ADR-0066 so Managed Session `Idempotency-Key` replay reaches the one
  root-aggregate command instead of being intercepted by management audit.
- Kept durable HTTP-attempt audit identity on `X-Request-ID` (or a generated id),
  eliminating the overlapping middleware dedupe authority while preserving
  fail-closed explicit audit-id replay.

## 2026-08-12 — Managed toolset documentation and E2E use the eight-member authority

- Corrected ADR-0037 and the SDK capability E2E to follow the canonical
  `AGENT_TOOLSET_TOOL_IDS`: `web_search` is the eighth official toolset member.
- An unconfigured WebSearch provider is projected as one disabled toolset
  override, preserving the single plugin/provider execution path rather than
  inventing or implying a second custom-search representation.

## 2026-08-12 — Session continuation admits before Runtime recovery

- Amended ADR-0066 so cold pending reads project directly from the committed
  `AwaitReason`; they no longer rebuild a Runtime context or Environment to
  repeat tool classification.
- Consolidated ordinary messages and tool continuations on one
  admission-before-activity ordering. A replacement process now advances the
  realization lease before resuming an awaiting Run and before opening its
  billable Running interval.

## 2026-08-13 (Hosted application MCP credentials remain Vault-owned)

- Added one idempotent create-or-rotate command over the existing Credential/Vault
  WAL and CAS for trusted hosted application MCP bearers.
- Kept Managed Session creation and MCP normalization authoritative; hosted
  applications retain no local credential or Session fallback.

## 2026-08-13 (Hosted governance credentials reuse canonical CRUD)

- Extended the existing Credential CRUD with stable operation identity lookup
  and exact reference validation for hosted governance products.
- Kept generic business credential material exclusively in Awaken's existing
  Credential repository and SecretStore; no Flow-local Vault or provider type
  was added.

## 2026-08-13 (Hosted credential references revalidate from durable identity)

- Corrected generic hosted reference validation to consume only the durable
  source id and exact Workspace/provider retained by the Resource.
- Kept the operation key on create replay/lookup only, avoiding a second
  product-owned identity mapping after restart.
