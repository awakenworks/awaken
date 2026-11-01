# ADR-0104: Memory Copy Reconciliation Adopts an Identical Durable Head and Refuses a Lost Copy Directory

- Status: Accepted
- Date: 2026-11-01

## Context

A writable Memory Copy materializes each durable head into the Sandbox and
records its `(id, version, content_sha256)` as materialization evidence frozen
in the Session aggregate. Terminal cleanup (`reconcile_terminal_memory`) reads
the Copy directory through `LocalSandbox::list_files` and calls
`MemoryStoreMounter::reconcile_recovered_copy`, which rebuilds its snapshot
from that original evidence and compare-and-swaps each changed file against the
materialized generation. A losing write is preserved with
`preserve_copy_conflict` before the Sandbox may be disposed.

Two product gaps remained in T29-C02:

- **R2r.** A Host that crashes after the first write-back commits but before
  `acknowledge_memory_reconciliation` makes its successor replay the same
  reconciliation from the original evidence. The update CAS then fails against
  the already advanced head and the replay preserved a conflict whose candidate
  is byte-identical to the durable head. The create path already adopted an
  identical durable file; the update path did not. The spurious proposal is
  permanent (there is no resolve or discard operation) and consumes the per
  store proposal capacity, after which `preserve_copy_conflict` fails with
  `AtCapacity` and terminal cleanup cannot dispose the Sandbox.
- **NC.** The neutral file scanner reads a missing subtree as empty. A Copy
  directory deleted before write-back therefore looked like "every file was
  locally deleted" and reconciliation would delete every durable head it was
  materialized from.

Earlier tests pinned the conservative R2r behaviour (copy_lifecycle R5 and R9,
writeback_faults UPDATE): an old generation with identical bytes was retained
as a proposal because it cannot prove the identical head is its own commit
rather than another writer's identical write.

## Decision

### D1 An update whose CAS loses adopts a durable head with identical content

When `update_materialized` fails with `Conflict` or `NotFound`, reconciliation
reads the current head. If its `content_sha256` equals the candidate, the head
is adopted into the snapshot and no proposal is preserved; otherwise the
existing "durable head changed" proposal is preserved unchanged. This is the
rule the create path already applies, so create, update and delete replays are
all idempotent.

Nothing is written on the adoption branch, so a concurrent head is never
clobbered (`memory.stale_cas_never_clobbers_live_content` still holds) and a
different winner is still retained as a conflict (R6, the stale local edit and
stale local delete rules are unchanged).

### D2 A materialized Copy directory that is missing is gone, not empty

`LocalSandbox::list_files` for a materialized Copy mount path returns
`SandboxError::gone` when that directory is not a directory on the exact root
incarnation, the same rule it already applies to a lost root. Terminal cleanup
then stays pending instead of authorizing deletion of every durable head.

## Consequences

- copy_lifecycle R5 and P3 and writeback_faults UPDATE: committed identical
  bytes are adopted; only a different winner is retained.
- copy_lifecycle R9 (two concurrent identical updates) converges to two
  successes at version 2, matching R8 for identical creates. The accepted
  trade-off: when another writer happens to store exactly the candidate bytes,
  no proposal is kept. That proposal would carry no information (it equals the
  head) and Copy write-back records no actor, so no attribution is lost.
- The ignored reproducers `repeated_reconciliation_preserves_no_spurious_conflict`
  (R2r) and `a_cleared_copy_directory_cannot_be_written_back` (NC) run.
- If exact write provenance is ever required (distinguishing an own commit from
  an identical foreign write), record the reconciliation operation identity on
  the Memory version and compare it instead of content; that needs a Memory
  store schema change and is not part of this decision.
- Retained proposals still have no resolve or discard operation and no
  retention policy; that is a separate product capability.
