# Wiki Update Log

## 2026-08-28 — Close targetless credential and MCP drain authority

- Made Provider, A2A, and MCP publication carry exact catalog/origin/canonical-
  URL targets through the existing Credential Source and Access path.
- Made the frozen Environment `mcp_holder` the only holder decision and retained
  one expected-revision migration for legacy application-MCP rows.
- Closed new Runtime calls before cancelling busy calls and acknowledged
  `Removed` only after local Run/process quiescence; external side effects that
  were already accepted remain an explicit operational boundary.

## 2026-08-28 — Bind package realization to one Open structural verifier

- Kept the existing package recipe, Kubernetes emitter, generic realization
  digest, Sandbox name owner, Coordinator build row/lease, and Registry digest
  as their sole authorities; no CLI, JSON parser, hash path, ledger, or retained
  proof store was added.
- Made the immutable package ConfigMap yield its API UID before Build and
  image-check Jobs are stamped. ConfigMap and Job `409` reuse now requires the
  exact realization and normalized projection rather than treating conflict as
  success.
- Added one versioned typed verifier for exact ConfigMap, optional fresh Build
  Job/Pod, mandatory image-check Job/Pod, immutable digest, and canonical cold
  Sandbox cardinality. Wrong UID/owner/spec/status/digest, copied annotations,
  duplicates, and competing incarnations fail closed; Registry recovery alone
  may omit both Build UIDs.
- Kept the proof secret-free and bounded. Successful Build and image-check Jobs
  remain observable for their TTL, while Coordinator and the Registry retain
  all durable execution authority.
- Classified the result as structural evidence only. A composing Cloud release
  still needs a separate admission/RBAC gate proving exclusive writes by its
  exact pinned Open Worker identity and the Kubernetes Job controller; without
  that gate there is no product-source proof. This record does not claim a live
  cluster deployment.
## 2026-08-28 — Close the Sandbox signature/attestation retry boundary

- Kept the release workflow, provenance module, registry resolver, and static
  checker as the only publication owners; no second schema, ledger, publisher,
  or compatibility path was added.
- Classified the image signature independently from the Open provenance
  predicate. Exact pinned-Cosign absence permits one signature write followed
  by a registry requery; one exact verified signature is reused, while
  malformed, multiple, mismatched, or query-error evidence fails closed.
- Made a crash after signing but before attesting converge as `S1/P0`: retry
  performs no second signature write and creates only the missing predicate.
  Predicate presence never substitutes for a signature, and an existing release
  tag still requires both exact facts plus a final immutable-tag read.
- Extended the causal self-test and publisher mutation matrix for exact absence,
  bundle/legacy representations, duplicate or drifting evidence, missing
  post-sign requery, and signature-bypass attempts. This record does not claim
  a live GHCR publication.

## 2026-08-28 — Prove Sandbox staging before release-tag promotion

- Kept the existing Sandbox build script, release workflow, strict predicate,
  and validator as the only build, publisher, and provenance owners; no second
  publisher, ledger, schema, or compatibility path was added.
- Made the workflow resolve a pre-existing semantic tag before any build and
  reuse it only with exact OCI labels, exact current repository/workflow
  SHA/ref/push-trigger keyless proof, and exactly one verified predicate.
  Missing or conflicting proof now rejects the preseeded tag instead of signing
  it or treating labels as authorization.
- Moved new publication to a run-scoped staging tag. The workflow resolves and
  proves its immutable digest first, then promotes the same manifest to the
  semantic tag only after a write-before comparison, then resolves the tag
  again and requires the same exact digest and OCI identity.
- Made retries closed around the promotion boundary: a pre-promotion crash
  leaves no release tag and an identical digest can reuse its proof; a
  post-promotion retry reuses the already-proven immutable digest. Consumer
  validation remains exactly-one and fail-closed.
- Added one executable registry resolver decision table and a repository-wide
  writer inventory. Static gates reject unsigned preseed, alternate publisher
  primitives, extra release tags, weakened workflow claims, incomplete labels,
  pre-proof promotion, and tag/config drift. This record does not claim a live
  GHCR publication.

## 2026-08-28 — Bind explicit Repository publication to terminal cleanup

- Kept `SessionCleanupOperation` as the sole terminal-effect authority: one
  explicit release freezes the exact active writable Repository input plus its
  caller-approved branch/full commit in the archive root CAS; no queue,
  registry, Resource generation, or protocol-local saga was added.
- Ordered every delegated child receipt before the root-only publication, made
  the publication receipt durable through the Session root CAS, and exposed the
  ordinary root cleanup only afterward so retries cannot lose the working tree.
- Reused the one `RepositoryRealizer` and Repository binding verifier for local
  and external Workers. Terminal authorization uses the current realization
  lease and aggregate-derived command rather than a fabricated Run claim or a
  mutable Worker catalog read.
- Preserved source Repository identity separately from direct/Gateway transport;
  credential bytes and mediated capabilities remain effect-local and never
  enter the publication intent or receipt.
- Made absent-ref creation lease-protected, exact remote state an identical
  receipt replay, and a different remote commit a fail-closed non-overwrite.
- Preserved exact v1 cleanup JSON and fingerprints when publication is absent.
  This documentation record does not claim deployment or live E2E validation.

## 2026-08-27 — Freeze profiled Session mutation authority at creation

- Added the persisted Managed/Frozen/FileResources policy to the immutable
  Session baseline and kept ADR-0066 as the sole post-create mutation owner.
- Kept the ordinary Managed wire shape, authoring behavior, omitted policy and
  inherited-prompt defaults, and historical fingerprints compatible. A typed
  prompt selection now preserves inherit, explicit clear, and exact replacement;
  maintenance reuses the existing explicit cutover instead of inferring old
  rows during rolling deployment.
- Made each private profiled command resolve its mode, direct Resource and
  Repository inputs, MCP candidates, and remaining creation inputs before the
  one original revision-1 Session insert; repository idempotency owns the replay
  receipt and the complete direct attachments are already desired there.
- Made the repository atomically classify create as Applied or Replayed. Only
  Applied continues into realization, activation, lifecycle wake, cleanup, and
  eligible WorkQueue dispatch; replay returns current durable truth without
  repeating an effect, while ActivationFailed remains a typed HTTP 409.
- Fenced create receipts to revision 1 and mutation receipts to their exact next
  revision. Mismatched operation/expected-revision reuse conflicts; dangling,
  ahead, or double identity fails as corrupt without synthesizing state.
- Removed post-create whole-manifest completion from the profiled creation
  model. Flow now projects one complete creation command, while Interactive
  products use the existing public item-level File verbs after creation.
- Split public MCP replacement from typed credential lifecycle adoption.
  Frozen and FileResources reject public MCP authoring, but the existing Vault
  lifecycle may still apply a same-source monotonic revision or revoke its
  affected attachment without broadening topology.
- Limited FileResources to File binding attach/replace/delete, froze all
  non-File inputs and exact Skill pins, froze all Resource changes for Frozen,
  and rejected profiled Repository credential rotation and private whole-
  manifest replacement before any Vault or lowering side effect.
- Kept the beta cutover full-stop and forward-only: old profiled metadata/default
  receipts are neither adopted nor backfilled, their deterministic retry returns
  409, and Flow creates a new post-cutover identity. Ordinary public Managed
  metadata replay stays compatible.
- Made Session-created Repository Registry and Vault work explicit
  `Applied | Replayed` participants in the existing root command. Pre-root
  failure consults durable adoption and compensates only unreferenced Applied
  work; after adoption the existing terminal saga requires exact Managed/Profiled
  namespace plus owner metadata, preserves shared definitions, and retires an
  owned inline credential by exact source revision through the canonical Vault
  archive/material path.
- Routed ordinary File item create/delete through the same complete-manifest root
  CAS with the revision read before derivation. Concurrent losers return `409`;
  neither item verbs nor whole-manifest replacement rebase over the winner.
- Made successful item deletion and whole-manifest omission persist removed
  Session-owned Repository retirement intent in the existing Session Resource
  root. The one reconciler consumes it after local or external activation, on
  exact receipt replay, during terminal cleanup, and after restart. Until its
  cleanup CAS succeeds, same-id reintroduction conflicts and the intent remains
  a Registry/Vault retention edge. Durable owner metadata is verified before
  any Vault effect; owned inline credentials retire by exact revision before the
  Repository, while shared, markerless, and already-absent definitions remain
  side-effect-free no-ops.
- Added the additive V2 actual MCP credential-source dependency index without
  changing the V1 checksum or reinterpreting Vault membership. Session root
  writes maintain it transactionally; SQLite/Postgres startup rebuilds it from
  canonical roots before serving, and Vault rollout selects exact Workspace plus
  source. The beta writer cutover remains full-stop; this record does not claim a
  deployment or live E2E result.

## 2026-08-26 — Bind credential source metadata to exact target and usage

- Extended the existing Credential source row and admin create/read/CAS-rotate
  authority with one optional secret-free descriptor; no store, catalog, or
  provider-specific credential aggregate was added.
- Made each descriptor declaration an exact target identity plus canonical
  usage without duplicating usage in executable access, normalized Repository
  audiences to one exact HTTPS Git origin, and repeated descriptor/material/
  expiry validation at the existing pinned materialization edge.
- Closed descriptor support to Repository, HTTP-effect, signature-verification,
  and Extension consumers; Provider/MCP remain legacy-only until their existing
  compilers carry targets.
- Kept Git on typed HTTP Basic and Connector API credentials scalar; one pure
  Vault compiler owns source-to-access admission, and each clone/publish effect
  reopens the exact pin rather than retaining plaintext or an old capability.
- Required positive revisions and expected-revision rotation/retirement;
  targetless mounts and unsupported Provider/A2A selection reject described
  sources instead of treating metadata as ambient authority.
- Consolidated legacy Provider and A2A source-row publication through the same
  Vault compiler with typed claim-time holder deferral; startup selection,
  Existing-provider discovery, and readiness reuse the canonical executable
  Provider-supply predicate, so described Extension/HTTP-effect credentials and
  legacy environment rows cannot be reinterpreted as model-provider material.
- Removed the read-latest material rotation wrappers and legacy Repository token
  ingress, leaving the existing Vault WAL/CAS as the sole replacement owner.
- Removed speculative dual-use token, issuer, and probe contracts until a
  production caller closes those ports.

## 2026-08-25 — Reuse the Session activity fence for every public Run protocol

- Consolidated AI SDK, AG-UI, and A2A Run/resume execution into the existing
  SessionApplication-owned durable Running/Idle activity boundary.
- Kept Hosted Worker MCP on its private service endpoint and retained the
  running-Run authorization rule; no public callback, protocol lifecycle store,
  or permissive fallback was added.
- Added a minimal regression that pauses Runtime execution and proves durable
  Running is visible until the exact call settles, including failure cleanup.

## 2026-08-24 — Bind beta cutover proof to the sole Session supervisor

- Extended the accepted Session-runtime-interval amendment with the exact
  post-repair Event-batch cutover seam: one final canonical repository scan
  publishes a process-local generation and three secret-free aggregate counts.
- Reused the existing Coordinator admin listener for per-candidate reads. A
  final-scan failure retains the preceding generation, and deployment remains
  closed until every candidate advances beyond its post-old-writer baseline
  with all three counts zero.
- Added no Cloud database scan, readiness inference, log-time heuristic,
  scheduler, migration store, or Session-id projection.

## 2026-08-24 — Publish the canonical Sandbox image from Open

- Kept `deploy/images/sandbox/build.sh`, its generated ACP contract, production
  Dockerfile, and runtime acceptance as the only image-build path.
- Added one protected semantic-tag workflow that pushes the fixed GHCR
  repository, then signs and attests only the resolved immutable digest through
  GitHub Actions OIDC.
- Made the Open-owned strict predicate and validator the only provenance schema;
  composing platforms verify it and retain its canonical digest but cannot
  build, republish, or add a parallel parser.
- Added static causal gates that reject a second workflow or local publisher,
  mutable actions, mutable image coordinates, alternate triggers, private keys,
  and weakened identity or issuer checks.

## 2026-08-24 — Correlate the existing Kubernetes package release path

- Added secret-free recipe fingerprint and destination annotations to the
  existing BuildKit Job and Pod template and reused them only on the existing
  package-destination image-check Job/Pod. A crash retry can therefore correlate
  the kubelet digest after the Build Job is gone; base and general probes do not
  claim package provenance. Coordinator build rows and the termination digest
  remain the only lifecycle and result authorities.
- Added the original neutral Sandbox scope and exact resolved image to the
  existing Pod creation seam. A bounded deployment observer may join it to the
  mission Session only when warm capacity is disabled and the scope equals the
  exact Session id, then compare the BuildKit digest, Pod image, and kubelet
  imageID without a Session map.
- Kept that optional evidence under a small additive size budget; an arbitrary
  overlong generic Sandbox scope remains runnable and yields no proof rather
  than leaking an adapter hash as Session identity.
- Kept proxy values, registry authentication, image-pull Secret names, recipe
  bodies, and the adapter-local Kubernetes runtime id outside the evidence; no
  store, route, scheduler, build queue, or Cloud-side runtime identity was added.
- Made the existing Ready-image checks consume the exact durable build demand
  plus stored digest. Kubernetes now re-derives the annotated package
  destination for every claimed, blocking, and periodic readiness check; a
  missing or different kubelet digest invalidates Ready and returns convergence
  to the sole claim/lease worker without running BuildKit from the check path.

## 2026-08-24 — Retain canonical Managed Session runtime intervals

- Kept the Session root CAS as the sole aggregate owner and retained closed
  Running intervals plus exact Event-batch admission revisions there; the
  delivery outbox remains delete-after-delivery and no Managed event store was
  added.
- Rebuilt Managed warm/cold history from retained inbound commands, Runtime
  lifecycle/message/state commit facts, exact processed-entry anchors, interval
  usage, and historical Awaiting audit targets. Stable interval identities
  replace the disposable latest-terminal aggregate bracket; terminal events wait
  for the existing cleanup quiescence seam's persisted Runtime high-water.
- Required a beta maintenance cutover that stops all old Coordinator writers
  before the new aggregate fields are written. New-reader defaults support old
  rows but do not make mixed old/new binaries safe; complete history begins with
  newly created post-cutover Sessions.

## 2026-08-24 — Reuse canonical hosted Session identity and credential adoption

- Exported the existing Managed Session create-idempotency address derivation
  and made the server itself consume it; durable Session replay remains the
  authority.
- Extended the existing application MCP credential receipt with the existing
  rollout adoption state and synchronous exact-event delivery; pending work
  remains on the sole supervised outbox path, and HTTP replay uses bounded
  primary-id lookup instead of enumerating that outbox.
- Fenced delayed application-MCP retries with a required caller-monotonic
  generation encoded in the existing material identity; legacy generation-zero
  rows upgrade through the same WAL/CAS, with no receipt table or counter.
- Added no metadata identity, Session mapping, credential store, route,
  scheduler, lease, or compatibility fallback.
- Bumped the existing hosted runtime-surface profile to schema v2 and projected
  Coordinator's canonical application-access maximum TTL through the CLI
  composition root, preserving the legacy incumbent ceiling during durable
  cutover; no second command, DTO, constant, or Control dependency was added.
- Extracted the existing application-capability issuance policy from its HTTP
  handler into one transport-neutral Coordinator use case, so product relation
  adapters reuse the same Session validation and durable mint without exposing
  the store primitive or adding a second issuer.
- Consolidated `web_fetch` and `web_search` on their configured-plugin owner:
  publication, root Session, and child realization now reuse the same execution
  policy validator; provider-server routes fail closed when they cannot enforce
  domain, content, or location policy. Removed the obsolete static Web executor,
  constructor, descriptor implementation, and empty hand-tool compatibility
  surfaces instead of retaining aliases.
- Closed the host WebFetch redirect gap at that same policy edge: direct fetch
  rejects redirects before a second request while domain policy is active, and
  Gateway-routed providers fail configuration until their target redirect
  enforcement is provable; no Cloud-side policy copy was added.

## 2026-08-23

- Recorded the shared multi-provider catalog and exclusive host/provider-server
  realization contract for the existing `web_search` and `web_fetch` builtins.

## 2026-08-19

- Separated Awaken's Viewer/Builder/Administrator product access levels from
  hosting subscription capacity. Added one least-privilege hosted Builder
  Workspace role and reused the existing full Run lifecycle role instead of
  duplicating an equal Runtime role.
- Kept organization billing and machine integration roles outside the human
  product role catalog; the product profile remains the only action matrix.

## 2026-08-18

- Replaced automatic store-only retry dead-lettering with one drainer-owned,
  exact-claim Worker terminalization path; manual quarantine remains separate.
- Bound caller-owned Run ids to the canonical dispatch fingerprint on the
  existing durable completion tombstone; no collision registry or second Run
  authority was added.
- Made different, ineligible, concurrent, and historically unverifiable
  payloads fail closed while trace-only retries preserve first-accepted options.

## 2026-08-15

- **Consolidation**: Amended ADR-0061 so Control and Resources retain distinct
  domain authorities while their same-Workspace IAM vocabulary is emitted once
  as `awaken.workspace`; hosted roles now express stable intent and concrete
  scope remains solely in IAM bindings.
- **Migration**: Required activation and binding proof before PEP cutover, then
  removal of superseded bindings and CAS retirement of both legacy profile
  heads; no request-time fallback or permanent dual-profile path remains.

## 2026-08-14

- Consolidated canonical Sandbox writable roots in the open container adapter
  so Kubernetes projection and closed checkpoint drivers cannot drift.
- Bound active PVC UID evidence into each Pod realization, fenced intentional
  claim deletion, and made synchronous partial creation roll back only the
  resources created by that attempt.
- Exposed the runtime-owned ACP, backend-native, and XDG configuration-home
  contract so checkpoint decorators exclude provider caches without copying
  process defaults or agent-specific path lists.

## 2026-08-13

- **Contract**: Built-in platform-held HTTP effects now freeze each credential
  material field's exact header, query, or RFC 6901 JSON-pointer destinations in
  `CredentialUsage`; single-secret, structured, OAuth, and malformed-placement
  shapes fail closed before Gateway I/O.
- **Consolidation**: Hosted Connector effects now select the neutral
  `PlatformRelay` realization in a trusted Gateway process that composes the
  canonical Credential repository, SecretStore, and pinned materializer. No
  plaintext credential RPC, copied Vault, or false cryptographic-envelope
  claim was introduced.
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

## 2026-08-23 — Local Cloud login and Web tools reuse their canonical owners

- Made desktop reauthentication a live capability source over IAM's one OAuth
  client and credential cache; the Console coordinates only secret-free status.
- Consolidated hosted and signed-in-local Cloud WebSearch/WebFetch on one open
  Gateway-routed builtin adapter. Cloud supplies exact route grants while the
  existing provider catalog, tool ids, Runtime operation identity, and Gateway
  remain authoritative.

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

## 2026-08-14 — Reuse the Session binding for active continuation volumes

- Extended the one Kubernetes Sandbox realization with a deterministic,
  non-Pod-owned continuation PVC; no PVC registry or filesystem scan was added.
- Worker failure preserves the live Pod, terminal-Pod recovery reuses the PVC,
  and only the existing explicit Sandbox disposal path deletes active storage.
- Kept the generic checkpoint store/provider contract and Session Environment
  state as the sole hibernation and restore authorities.

## 2026-08-15 — Admit the complete Session credential projection before effects

- Moved the frozen Session runtime projection schema into the existing
  run-ingress contract and removed the Runtime Host's duplicate private shape.
- Made broad selection and exact claim admit MCP holder/access pins through the
  same credential kernel used by inference, while keeping MCP generations and
  receipts Session-owned.
- Classified material-source and refresh unavailability into the existing
  retryable claim lifecycle; invalid projections and policy remain absorbing.

## 2026-08-15 — Hosted sealing reuses the exact Vault compiler

- Added one deployment issuer port after active-source, Workspace, revision,
  holder, usage, and target-binding admission.
- Kept `CredentialAccess` and the pinned materializer authoritative; hosted
  transports cannot add a plaintext API, copied Vault, or unsealed fallback.

## 2026-08-15 — Make the hosted route the first authorization scope

- Removed persisted Workspace preferences from the request-addressing boundary;
  the exact `/w/{workspace}` route now wins before the first hosted API request.
- Reused Cloud `/entry`, the existing Workspace-context probe and the existing
  PDP; no product login, session store or authorization path was added.
- Classified expired bearer (`401`), exact deny (`403`) and infrastructure
  failure before mounting the product router, preventing both stale-scope
  requests and authentication loops.

## 2026-08-15 — Deliver exact Agent publications through Session realization

- Extended the existing executable Agent profile source and frozen Session
  projection with the complete exact publication selected by the baseline.
- Kept Control authoritative and Workers authority-store-free; the Coordinator
  transports a rebuildable snapshot and conflicting Run claims fail closed.

## 2026-08-15 — Separate Worker liveness from Sandbox-backed observations

- Kept Worker observations and warm capacity on their existing background
  reconcilers, but made them start immediately after an evidence-empty Ready
  heartbeat instead of blocking the process readiness boundary.
- Added typed retained/ephemeral filesystem continuity to the neutral Sandbox
  plan; Kubernetes still owns one realization path and skips continuation PVCs
  only for disposable probes and child environments.
- Preserved retained Session environments and `DurableRequest` placement while
  missing dynamic evidence continues to reject only dependent Runs.
- Unified the final Kubernetes Pod volume projection with the same typed
  retained/ephemeral decision used by claim creation, removing stale references
  to deliberately omitted probe claims without adding another storage path.

## 2026-08-15 — Preserve terminal Session truth at the Worker boundary

- Added one typed disposition to the existing Session realization control
  contract and reused it across embedded and HTTP Worker paths.
- Kept `NotReady` and transient authority failures on their existing retry
  paths, while terminal durable Session truth now absorbs the Run instead of
  creating an unbounded claim/relinquish loop.
- Added no Session, WorkQueue, Run, or retry authority; a later business attempt
  preserves the failed Session as history and creates fresh execution truth.
