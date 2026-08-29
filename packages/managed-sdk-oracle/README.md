# Managed SDK oracle

This package turns selected official TypeScript `@anthropic-ai/sdk` and Python
`anthropic` releases into deterministic compatibility oracles and one Managed
behavior qualification graph. It is development and CI tooling; runtime code
does not depend on either SDK package.

The dependency direction is singular: the selected **current official SDK** is
the external compatibility expectation; Awaken's closed Rust request/response
types and routes are the internal implementation of that contract. Everything
else is derived evidence:

- `contracts/anthropic-managed/upstream-oracle.generated.json` records the
  normalized official TypeScript SDK routes and scoped declaration/runtime
  fingerprints;
- `contracts/anthropic-managed/python-upstream-oracle.generated.json` binds
  every selected Python change point to its exact PyPI wheel SHA-256, normalized
  operation inventory, handwritten resource helpers, explicit Managed library
  exports, and scoped source fingerprint. It walks the exact symbol-level import
  closure from every Managed resource into the generated DTO/type graph;
- `contracts/anthropic-managed/canonical-wire.schemas.generated.json` records
  the wire schemas generated from the Rust implementation and is compared to
  that oracle rather than treated as a competing protocol definition;
- generated coverage/catalog/type gates prove that callers and behavior tests
  stay on the same path. The current Python oracle must retain the reviewed
  operation identity relationship to TypeScript, and wildcard, unresolved, or
  ambiguous imports fail closed.
- Cloud qualification executes the same named SDK anchors against a deployed
  product from Cloud's exact pinned Open checkout. It is runtime/deployment
  evidence, never another contract authority; Cloud does not copy SDK versions,
  clients, or scenarios.

Two generated verification artifacts prevent the implementation and its
evidence from drifting from the external expectation:

- `contracts/anthropic-managed/operation-coverage.generated.json` maps every
  current SDK operation and reviewed SDK-absent route to one exact executable
  Rust behavior test and the exact SDK compile fixtures.
- `fixtures/generated/*.ts` compile every discovered operation against every
  pinned SDK anchor. `fixtures/user-profiles-change-point.ts` additionally
  checks the intentional response-type transition at the User Profiles anchor.
- `test/sdk-wire-lifecycle.test.ts` invokes every current scoped SDK operation
  through a recording transport. This includes the Managed Beta namespaces and
  the GA Files, Models, and Skills namespaces. Strict TypeScript checks the
  arguments while runtime assertions bind the exact method, normalized path,
  `beta=true` transport selector, and capability set.
- `test/sdk-transport-resilience.test.mjs` runs every supported TypeScript
  anchor and the admitted candidate through one error/retry contract. It owns
  the exact 400/401/403/404/409/413/422/429/500/529 subclasses and headers,
  retry decision table including `x-should-retry`, byte-identical mutation
  identity, opaque cursor traversal, and caller-abort fencing.
- The declaration oracle includes the Managed helper entrypoints in addition
  to HTTP resource types. A separate pre-import ESM export inventory is checked
  against the executable current modules; adding or removing a helper inside
  an already-owned file therefore cannot reuse that file's behavior evidence.
  `test/managed-helper-contract.test.mjs` projects that inventory over every
  TypeScript anchor and the reviewed candidate. Every public export must belong
  to the closed behavior catalog and execute its developer-visible contract:
  Zod parsing, error predicates/backoff, helper lifecycle/abort, Session event
  accumulation, filesystem confinement and tools, persistent Bash state, Skill
  resolution/admission, and Memory helper invariants. A newly exported symbol
  therefore fails as unowned even when its entrypoint and file fingerprint were
  already known.
- The generated oracle also fingerprints the transitive `.mjs` dependency
  closure rooted at the supported resources, Client, Session accumulator,
  EnvironmentWorker, Agent Toolset, and typed helper entrypoints. Shared client
  implementation drift can therefore not hide behind unchanged operations or
  declarations; unrelated Messages/Organization modules remain outside the
  Managed claim.
- `e2e/conformance/run_managed_sdk_behavior_owners.mjs` executes every distinct
  real-process owner and records successful 2xx official-SDK HTTP exchanges.
  It projects the same owner graph over all 99/114/127 HTTP operations exposed
  by the exact 0.105/0.117.1/0.121/0.122 anchors, and replays all 127 operations
  plus that exact SDK's four or five generated helpers. A package-selection hook resolves
  unchanged canonical imports inside the selected exact package root; each
  receipt must carry that package's exact `x-stainless-package-version`, method,
  route, Beta/GA selector, and capabilities. Operation-local capabilities
  removed by a newer SDK are derived from the other admitted anchors and
  explicitly forbidden, so a caller-supplied legacy header cannot make a new
  generated request appear compatible. Files/Skills wire coordinates are
  projected from that package's generated operations, so the 0.122 post-GA
  DTOs are exercised without copying the 0.121 tests or accepting a union
  shape. Every successful JSON response is also checked against a recursive
  closed contract extracted from that exact package's adjacent `.d.ts`:
  required fields, optionality, nesting, nullability, arrays, unions, and finite
  literal discriminators are enforced. Literal observations cross the process
  boundary only as run-scoped keyed fingerprints, so evidence cannot retain a
  credential, metadata value, or user payload. Page methods must additionally
  observe a non-empty item at least once. The only open JSON response positions
  are named `json-schema` and `tool-input` contracts from the official protocol;
  an `any`/`unknown` anywhere else, or an unchecked recursive response type,
  fails extraction before behavior evidence can be admitted. The same
  TypeChecker walks the complete non-transport request parameter closure for
  every anchor and candidate. It admits open request JSON only beneath a custom
  tool's named `input_schema`, treats `Uploadable` as the distinct multipart
  transport boundary, and rejects arbitrary `any`/`unknown`, unconstrained
  objects, or payloads disguised as transport `options`. A method present only
  in source text, an empty page, or a same-primitive wrong discriminator cannot
  satisfy qualification.
- `src/conformance/hosted.mjs` is the canonical deployed behavior runner. A
  product supplies only endpoint credentials and fixture identities through
  environment variables. It executes positive Beta/GA lifecycles, all-operation
  negative/error probes, pagination, concurrent idempotency, retry identity,
  and SSE full-replay/deduplication. Every ingress and all-operation response
  must carry one scalar, run-unique request id and the exact authenticated
  Workspace identity; the runner compares those values only in memory and
  retains booleans in differential evidence. For every release client it also
  compares successful create/retrieve/update Session, Agent, Stats, and Usage
  field sets directly with Anthropic-owned reference fixtures, so two
  independently successful lifecycles cannot hide an extra or missing response
  field. The
  ordinary command permits an optional
  official-service differential for developer diagnostics; the release command
  requires all official-reference credentials and fails closed when they are
  absent or partial. It then runs the same positive SDK lifecycles against
  reference-owned Agent, Environment, Workspace, UserProfile, API, and WIF
  fixtures. The 139-route differential sweep remains single-owned and is not
  repeated in that lifecycle pass.
- `src/conformance/recovery.mjs` splits a Session/File/idempotency and active
  Tunnel/WIF scenario at a process-replacement boundary. Product orchestration
  runs `prepare`, replaces every serving process, then runs `verify` and
  `cleanup`; an in-memory cache cannot satisfy this gate. The private recovery
  record contains the Tunnel id and a SHA-256 witness of the rotated token,
  never the token itself, so verification proves secret persistence without
  turning qualification artifacts into credentials.
- `src/conformance/release.mjs` is the sole release-grade composition of those
  two drivers. It requires `AWAKEN_MANAGED_REPLACE_AND_WAIT_COMMAND` to replace
  the deployment and write versioned evidence to the injected
  `AWAKEN_MANAGED_REPLACEMENT_EVIDENCE_FILE`. The evidence must contain non-empty,
  unique, disjoint before/after serving-instance sets at the exact
  `AWAKEN_MANAGED_EXPECTED_REVISION`, bound to the canonical
  `AWAKEN_MANAGED_BASE_URL`, with the replacement set ready. The hook
  receives no Managed API or Tunnel credentials. Hosted differential, durable
  prepare, total replacement, recovery verification, and cleanup execute in
  that order. Every post-prepare failure attempts compensation; a cleanup
  failure preserves the private recovery/evidence directory and reports its
  exact path so the same fixture identities can be reconciled rather than lost.
- `e2e/conformance/managed_python_sdk_runtime_e2e.{mjs,py}` provisions the exact
  locked Python client closure in an isolated virtual environment and drives a
  real Awaken process. It owns Python-specific sync/async calls, cursor and SSE
  decoding, typed errors, multipart encoding, beta/GA resource handoff, and the
  standard-webhooks adapter. The companion helper driver executes the official
  poller, SessionToolRunner, EnvironmentWorker, accumulator, and Agent Toolset;
  a two-process scenario recovers Session/Event, Memory, File, and Skill facts
  from one durable deployment. Before those real-process slices, a recording
  transport invokes all 127 Python methods through both synchronous and
  asynchronous `with_raw_response` resources and requires their exact generated
  verb, normalized route, query selector, and beta-header set; a new required
  parameter fails closed until its fixture is reviewed. Both client modes share
  the same extracted operation ledger, fixture vocabulary, and assertions, so
  async drift cannot hide behind a duplicate inventory. The existing shared
  behavior owners continue to own service-domain semantics, so this client
  sweep does not copy their resource state machines.
- `managed_python_sdk_response_contract_e2e.py` closes the response half of
  that all-operation chain without introducing a second fixture authority. It
  projects every Python operation onto the reviewed TypeScript 0.122
  declaration, then automatically generates one-at-a-time MC/DC witnesses for
  every nested union branch, optional-field omission, closed field, finite
  literal, array item, map value, nullable response, page, binary body, and SSE
  wrapper. All 3,827 witnesses traverse both the real synchronous and
  asynchronous Python 1.2 generated clients, `httpx2` transport, media
  dispatcher, and response converter. The return annotation's Pydantic JSON
  Schema is independently normalized and compared property-for-property, so
  permissive `BaseModel` extra-field retention cannot impersonate a declared
  DTO. A structural verifier inductively proves that the generated corpus
  reaches every finite union branch, array item, object field, optional-field
  omission, and map value in the actual declaration graph. Parsed JSON must
  equal its input witness exactly; values inside an installed declaration also
  make every Pydantic serializer warning fatal. The sole reviewed upstream type
  variance is fail-closed: six Work
  operations share `BetaSelfHostedWork.data`, whose Python 1.2 annotation omits
  the official `healthcheck` union branch. The original two-branch wire
  witnesses still decode in both client modes, while the live Python poller
  helper separately drains both `healthcheck` and `session`; if Anthropic fixes
  the annotation or another operation inherits the variance, the exact ledger
  fails until it is deliberately reviewed.
- `managed_python_sdk_matrix_e2e.py` executes every earlier reviewed Python
  change point in its own SHA-256-qualified wheel root over one exact dependency
  closure. It re-extracts and compares the installed source/operation/helper/
  export and handwritten transport-source fingerprints, constructs every
  operation through both sync and async clients, projects one shared decision
  table through both clients to prove the canonical Python error subclasses,
  retry policy, and byte-identical idempotent retries, and projects the current
  declaration-derived response corpus onto every operation still exposed by
  that historical wheel. Every nested union/optional response witness traverses
  the wheel's own sync and async media dispatcher and generated converter; this
  proves old clients consume current service-line responses without duplicating
  fixtures. Values already covered by the older declaration must remain
  warning-free; later additive fields and variants may be unknown to the older
  type graph but must still round-trip every wire value exactly. This avoids
  incorrectly requiring old declarations to equal newer additive declarations
  or mistaking permissive but lossy parsing for compatibility. It also runs live
  Session/SSE and Memory lifecycles. Before
  either runtime driver sends a request, one
  shared installed-evidence adapter re-extracts that wheel's complete Managed
  source graph. The current wheel compares every DTO path and hash; historical
  wheels compare the same closure's exact count and fingerprint, avoiding
  thirteen copies of thousands of generated rows while still failing on any
  changed dependency byte.
  The generated SSE dispatch inventory records that the first 0.92 Managed
  wheel filters canonical Managed event names in its own generic parser; 0.100+
  must decode message and terminal replay. The last Beta-only and first GA
  Files/Skills releases own
  the two additional projection-cutover cells; the current 1.2 behavior remains
  owned by the deeper runtime/recovery driver instead of being run twice.

`config/anchors.json` selects exact SDK releases and assigns each a stable role.
`config/python-anchors.json` selects behavior-changing Python releases from the
first Managed Agents SDK through the current oracle; patch releases without a
Managed protocol, GA, helper, or transport change are intentionally not sampled.
`config/scope.json` declares the Managed Beta namespaces, the GA Files/Models/
Skills namespaces, and the small reviewed set of documented routes that are
intentionally absent from the selected SDK anchors. Beta and GA are separate
SDK transports over selected shared aggregates: the oracle retains the SDK's
`beta=true` selector so testing one projection cannot accidentally certify the
other. Operation inventories and type fingerprints must never be copied into
another hand-maintained manifest.

The generated current wire contract also owns the browser Session event catalog
and the Session plus nested SessionAgent properties/shape. Its TypeScript seam
exports the exact current official Session, request, response, event, page,
content, and usage types needed by browser consumers; the generated catalog
supplies runtime classification without a hand-maintained union. The JSON oracle
feeds the exact Session-response gate. The standalone E2E package's
`@anthropic-ai/sdk` dependency is an executable mirror only: `generate` and
`check` reject it unless it exactly matches the current oracle version.

## Updating an SDK anchor

1. Change the exact npm alias version in `package.json` and the E2E executable
   mirror. `config/anchors.json` keeps the stable role-to-module mapping.
2. Run `pnpm install` at the repository root.
3. Run `pnpm --filter @awaken/managed-sdk-oracle generate`.
4. Review route deltas and declaration fingerprint changes as API changes, not
   as formatting noise.
5. Run `pnpm --filter @awaken/managed-sdk-oracle test` and
   `pnpm --filter @awaken/managed-sdk-oracle check`.
6. Run this exact revision's hosted conformance command against every product
   deployment; products must not maintain their own SDK version aliases. A
   release pipeline invokes `pnpm --filter @awaken/managed-sdk-oracle
   conformance:release`, never the diagnostic-only `conformance:hosted` command.

For a Python anchor, update `config/python-anchors.json`, run
`pnpm --filter @awaken/managed-sdk-oracle generate:python`, review every
operation/source/wheel delta, then run the same `test` and `check` commands.
`check:python:online` re-downloads only the exact current wheel, verifies its
PyPI digest and extracted evidence, and applies the repository's common
minimum-release-age policy to newer registry releases.

The `check` command regenerates in memory and compares byte-for-byte. CI and the
contract generation script both call it, while `test` type-checks all compile
fixtures, so stale or unowned compatibility claims fail closed.

## Qualifying a newer candidate

`npm --prefix e2e run test:sdk-latest-canary` keeps promotion and verification
as separate decisions. A newer registry release remains in the repository's
minimum-release-age window, but a reviewed exact candidate alias is still run
through the compatibility proof. Its version pair, registry sha512 integrity,
installed version, complete Managed source/type/runtime delta, and executable
behavior ownership must all agree before candidate code is imported. An
unreviewed release therefore fails closed instead of being reported as merely
quarantined. The same proof can also be applied to an already provisioned exact
SDK package root while preparing its reviewed qualification:

```bash
ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT=/absolute/path/to/node_modules/@anthropic-ai/sdk \
  node e2e/conformance/sdk_latest_runtime_canary.mjs
```

The runner reads the candidate's generated operation source. For Beta Files and
Skills it requires every operation to agree on `beta=true` and its endpoint
capability, derives the historical-Beta or GA wire projection from that request
signature, strictly type-checks all changed methods against that exact package,
and rejects TypeScript module resolution outside that package root. Before any
candidate module is imported, the complete operation, declaration, and runtime
delta must match one reviewed version-pair fingerprint in
`official_sdk_candidate_qualifications.json`; every delta coordinate must belong
to exactly one named executable behavior owner. Reusing an allowlisted filename
with different bytes therefore fails closed before candidate code can run. It
then executes both Beta-namespace and GA lifecycles and injects only that exact
alias into the same Session/Native/ACP, resource-handoff, Memory/CAS, and Webhook
matrices used by the stable anchors. The runtime proof covers the
candidate's authentication header and retry behavior, exact Managed error
envelopes, anonymous/read-only/admin policy enforcement without denied-write
side effects, cursor traversal, rejected mutations, missing/deleted identities,
immutable version archives, cross-root identity, and a real process restart over
one durable store, including Session/Event, Memory, File, and Skill recovery.
A shared transport receipt encoder requires every discovered
Beta Files/Skills operation to reach a successful 2xx resource response with its
exact method, route, selector, capability, and official-SDK marker; 401/403
policy failures cannot impersonate resource-owner coverage. The runner also
executes every webhook parser exposed by that SDK. Changes in the Managed
runtime dependency closure additionally activate tests for Session SSE
accumulation and forward-compatible events, Agent Toolset registration and
bounded file reads, `setupSkills("latest")` after process replacement, bare-Blob
multipart admission, invalid-upload diagnostics, cross-realm aborts, configured
SSE logging, and exact SDK self-identification. It never chooses behavior from a
version-number table, uses type escape hatches, or silently falls back to the
installed current anchor. After the observation window, a passing candidate
proof still fails the release gate until that exact version is promoted to the
canonical current oracle.

The 0.121→0.122 change point intentionally records one upstream source-level
break: 0.122 removes the public `resolveSkillVersion` re-export from
`tools/agent-toolset/node`. Static runtime and declaration inventories prove
that it is the sole removed Managed helper; candidate compilation proves the
symbol is absent, while the real-process `setupSkills("latest")` recovery case
proves the replacement server-resolved version flow. This is an official SDK
API boundary, not an Awaken compatibility shim, and future export changes fail
closed until they receive their own explicit behavior owner.

Supplying the same exact candidate module and version to `conformance:hosted`
and every `conformance:recovery:*` phase also admits it to public-ingress and
process-replacement evidence. Those paths cover the candidate's GA and Beta
resource projections, User Profiles, pagination/idempotency/reconnect, and
Tunnel/WIF behavior. Recovery records the selected SDK role and version and
rejects a later phase run with a different package. Beta Files and Skills are
invoked without hand-authored capability headers, ensuring that the deployed
service receives the official SDK's generated 0.121 legacy-Beta or 0.122
GA-projection behavior rather than a test-masked approximation.
