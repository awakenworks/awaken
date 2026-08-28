# Managed SDK oracle

This package turns selected official `@anthropic-ai/sdk` releases into a
deterministic compatibility oracle. It is development and CI tooling; runtime
code does not depend on an SDK package.

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
  product. It is runtime/deployment evidence, never another contract authority.

Two generated verification artifacts prevent the implementation and its
evidence from drifting from the external expectation:

- `contracts/anthropic-managed/operation-coverage.generated.json` maps every
  current SDK operation and reviewed SDK-absent route to its Rust behavior-test
  owner and exact SDK compile fixtures.
- `fixtures/generated/*.ts` compile every discovered operation against every
  pinned SDK anchor. `fixtures/user-profiles-change-point.ts` additionally
  checks the intentional response-type transition at the User Profiles anchor.

`config/anchors.json` selects exact SDK releases and assigns each a stable role.
`config/scope.json` declares the Managed namespaces plus the small, reviewed set
of documented routes that are intentionally absent from the selected SDK
anchors. Operation inventories and type fingerprints must never be copied into
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
6. Update the Cloud qualification alias with the same stable role and exact
   version, then run its matrix suite against the product.

The `check` command regenerates in memory and compares byte-for-byte. CI and the
contract generation script both call it, while `test` type-checks all compile
fixtures, so stale or unowned compatibility claims fail closed.
