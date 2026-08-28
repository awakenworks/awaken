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
  operation inventory, and scoped source fingerprint. The current Python oracle
  must retain the reviewed operation identity relationship to TypeScript;
- `contracts/anthropic-managed/canonical-wire.schemas.generated.json` records
  the wire schemas generated from the Rust implementation and is compared to
  that oracle rather than treated as a competing protocol definition;
- generated coverage/catalog/type gates prove that callers and behavior tests
  stay on the same path;
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
- The generated oracle also fingerprints the transitive `.mjs` dependency
  closure rooted at the supported resources, Client, Session accumulator,
  EnvironmentWorker, Agent Toolset, and typed helper entrypoints. Shared client
  implementation drift can therefore not hide behind unchanged operations or
  declarations; unrelated Messages/Organization modules remain outside the
  Managed claim.
- `e2e/conformance/run_managed_sdk_behavior_owners.mjs` executes every distinct
  real-process owner and records completed, non-5xx official-SDK HTTP exchanges.
  It fails unless all current HTTP operations are observed with their exact
  method, route, Beta/GA selector, and required capabilities. A method present
  only in source text can therefore no longer satisfy behavior qualification.
- `src/conformance/hosted.mjs` is the canonical deployed behavior runner. A
  product supplies only endpoint credentials and fixture identities through
  environment variables. It executes positive Beta/GA lifecycles, all-operation
  negative/error probes, pagination, concurrent idempotency, retry identity,
  SSE full-replay/deduplication, and an optional official-service differential.
- `src/conformance/recovery.mjs` splits a Session/File/idempotency scenario at a
  process-replacement boundary. Product orchestration runs `prepare`, replaces
  every serving process, then runs `verify` and `cleanup`; an in-memory cache
  cannot satisfy this gate.
- `e2e/conformance/managed_python_sdk_runtime_e2e.{mjs,py}` provisions the exact
  locked Python client closure in an isolated virtual environment and drives a
  real Awaken process. It owns Python-specific sync/async calls, cursor and SSE
  decoding, typed errors, multipart encoding, beta/GA resource handoff, and the
  standard-webhooks adapter. Before that real-process slice, a recording
  transport invokes all 127 Python methods through `with_raw_response` and
  requires their exact generated verb, normalized route, query selector, and
  beta-header set; a new required parameter fails closed until its fixture is
  reviewed. The existing shared behavior owners continue to own service-domain
  semantics, so this client sweep does not copy their resource state machines.

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
   deployment; products must not maintain their own SDK version aliases.

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
then executes both Beta-namespace and GA lifecycles. The runtime proof covers the
candidate's authentication header and retry behavior, exact Managed error
envelopes, anonymous/read-only/admin policy enforcement without denied-write
side effects, cursor traversal, rejected mutations, missing/deleted identities,
immutable version archives, cross-root identity, and a real process restart over
one durable store. A shared transport receipt encoder requires every discovered
Beta Files/Skills operation to reach a real non-5xx resource response with its
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
