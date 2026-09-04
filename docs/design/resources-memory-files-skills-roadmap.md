# Resources, Memory, Files, and Skills Roadmap

This companion owns only the delivery sequence, explicit exclusions, and
guardrail index for the canonical
[Resources, Memory, Files, and Skills design](resources-memory-files-skills.md).
It does not define another domain model, lifecycle, or source of truth.

## First Vertical Slice

1. Introduce typed bindings and one `SessionInputResolver` without changing
   storage backends.
2. Preserve access, reject mount collisions, generate prompts from the effective
   manifest, and remove the second Host merge.
3. Make File materialization binary-safe and keep Session working copies outside
   FileStore mutation.
4. Add Memory/Repository config versions and platform-managed Repository ids.
5. Route Memory mount/API/recall/extraction/history/redaction through one
   `MemoryRepository`; remove blob/harvest.
6. Add durable activation reconciliation and per-kind reclamation.

## Non-Goals

- no Git commit or tree pinning;
- no Memory content snapshot or entry-version pinning;
- no File version layer above content identity;
- no exact Repository replay after cold sandbox reconstruction;
- no Project or WorkUnit concept in awaken resources;
- no IAM policy language inside resource services;
- no generic store abstraction combining File, Memory, and Repository;
- no implicit deletion of a remote Git repository.

## Guardrails

G3, G4, G8, G9, G13, G14, G21, G27, G37, G38, and G39 in
[INVARIANTS](../INVARIANTS.md).
