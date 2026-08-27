# Managed SDK oracle

This package turns selected official `@anthropic-ai/sdk` releases into a
deterministic compatibility oracle. It is development and CI tooling; runtime
code does not depend on an SDK package.

There are three independent authorities:

- `contracts/anthropic-managed/canonical-wire.schemas.generated.json` is the
  Awaken wire contract generated from Rust types.
- `contracts/anthropic-managed/upstream-oracle.generated.json` records the
  normalized official SDK routes and scoped declaration fingerprints.
- Cloud qualification executes the same named SDK anchors against a deployed
  product. It proves behavior; it does not redefine either contract.

`config/anchors.json` selects exact SDK releases and assigns each a stable role.
`config/scope.json` declares the Managed namespaces plus the small, reviewed set
of documented routes that are intentionally absent from the selected SDK
anchors. Operation inventories and type fingerprints must never be copied into
another hand-maintained manifest.

## Updating an SDK anchor

1. Change the exact npm alias version in `package.json` and the corresponding
   version in `config/anchors.json`.
2. Run `pnpm install` at the repository root.
3. Run `pnpm --filter @awaken/managed-sdk-oracle generate`.
4. Review route deltas and declaration fingerprint changes as API changes, not
   as formatting noise.
5. Run `pnpm --filter @awaken/managed-sdk-oracle test` and
   `pnpm --filter @awaken/managed-sdk-oracle check`.
6. Update the Cloud qualification alias with the same stable role and exact
   version, then run its matrix suite against the product.

The `check` command regenerates in memory and compares byte-for-byte. CI and the
contract generation script both call it, so stale generated artifacts fail
closed.
