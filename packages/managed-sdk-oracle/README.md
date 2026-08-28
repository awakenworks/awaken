# Managed SDK oracle

This package turns selected official `@anthropic-ai/sdk` releases into a
deterministic compatibility oracle and the single executable Managed behavior
qualification runner. It is development and CI tooling; runtime code does not
depend on an SDK package.

The dependency direction is singular: the selected **current official SDK** is
the external compatibility expectation; Awaken's closed Rust request/response
types and routes are the internal implementation of that contract. Everything
else is derived evidence:

- `contracts/anthropic-managed/upstream-oracle.generated.json` records the
  normalized official SDK routes and scoped declaration fingerprints;
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

`config/anchors.json` selects exact SDK releases and assigns each a stable role.
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

The `check` command regenerates in memory and compares byte-for-byte. CI and the
contract generation script both call it, while `test` type-checks all compile
fixtures, so stale or unowned compatibility claims fail closed.

## Qualifying a newer candidate

`npm --prefix e2e run test:sdk-latest-canary` observes the registry but never
downloads or executes a release during the repository's minimum-release-age
window. Once supply-chain policy has provisioned an exact SDK package root, the
same runtime proof can be applied before changing the canonical anchor:

```bash
ANTHROPIC_SDK_RUNTIME_PACKAGE_ROOT=/absolute/path/to/node_modules/@anthropic-ai/sdk \
  node e2e/conformance/sdk_latest_runtime_canary.mjs
```

The runner reads the candidate's generated operation source. For Beta Files and
Skills it requires every operation to agree on `beta=true` and its endpoint
capability, derives the historical-Beta or GA wire projection from that request
signature, and then executes both Beta-namespace and GA lifecycles. It also runs
every webhook parser exposed by that SDK. It never chooses behavior from a
version-number table or silently falls back to the installed current anchor.
