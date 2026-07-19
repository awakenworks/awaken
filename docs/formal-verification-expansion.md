# Formal verification expansion: 64 additional obligations

The first batch expanded the versioned safety ledger from 28 to 56 formalizable,
machine-linked obligations. The original 28 remain unchanged. Those additional
28 are grouped below by the production boundary they verify; the second batch
below raises the final total to 92.

## No distributed-architecture change required

- Tenancy (3): successful resolution never widens authority; every selector is
  checked; selector order cannot turn disagreement into authorization. The only
  code-shape change is a representation-independent `Eq` decision kernel called
  by the production `ScopeId` adapter, allowing Kani to avoid unbounded `String`
  internals.
- Authorization (2): the HTTP-method mapping is total; only `GET`/`HEAD` map to
  read; `Deny` and `RequireApproval` both collapse fail-closed.
- Lease/egress (3): credential expiry is capped by both token TTL and lease;
  revoked or expired leases deny egress; reap causes obey the fixed priority.
- Compaction (2): a fold preserves the requested suffix; a fold exists exactly
  when the threshold is crossed and the prefix is non-empty.
- Live inbox (3): message identity is monotonic; retired messages never reappear;
  close is absorbing.
- Streaming checkpoint/recovery (4): the durable watermark never rolls back;
  crash preserves durable truth; recovery never starts before the committed
  prefix; terminal completion clears the resume checkpoint.

These 17 properties were already expressed by pure kernels or explicit state
machines. They needed proof harnesses/models and production evidence, not a new
storage topology.

## Architecture change required and implemented

- Remote tool relay (4): stable operation identity, at-most-once admission,
  result/operation correlation, and fail-closed indeterminate recovery. Transport
  correlation is now separate from the stable operation id, and a durable
  operation ledger owns claim/result recovery.
- Managed session ownership (2): session data and owner are one atomic repository
  write; a visible session row always has an owner. The former `save` then
  `set_owner` sequence was replaced by `save_owned` in memory, SQLite, and
  PostgreSQL.
- Circuit breaker (3): half-open capacity, generation fencing, and abandoned
  probe reopening. `check() -> ()` was replaced by a generation-bound RAII permit,
  so success/failure/cancellation cannot be attributed to another probe cycle.
- Config authoring (2): generation is monotonic and a stale writer never applies.
  The authoring row now carries `generation`; SQLite and PostgreSQL implement one
  statement compare-and-set, and the HTTP projection accepts/returns generation
  and reports conflicts as `409`.

These 11 properties could not be proved honestly over the old check-then-write or
name-only APIs; the atomic capability/generation had to become part of the
production architecture first.

## Verification boundary

The 56/56 metric is safety coverage, not a claim of end-to-end exactly-once
effects. Eventual network delivery, correctness of external database engines,
LLM semantics, and exactly-once third-party side effects remain environmental.
Webhook commit-to-enqueue atomicity would require a transactional outbox in the
same database transaction as the lifecycle fact; it is intentionally not
mislabelled as proved by delivery retry tests alone.

## Second batch: 36 additional obligations

The second batch raises the ledger from 56 to 92 formalizable obligations. It
adds eight finite-state TLA+ models and thirteen Kani harnesses over production
pure kernels.

### Architecture adjusted and verified

- Webhook outbox (4): lifecycle emitters use stable fact ids; SQLite/PostgreSQL
  persist secret-free pending rows; successful delivery retires a row and failed
  delivery leaves it pending. The proved commit boundary is the outbox row itself.
- GDPR erasure saga (4): completed erasers, removed count, accountability stamp,
  and completion are durable checkpoints. Retry resumes after the last completed
  target and never counts an eraser twice.
- Credential creation (2): secret stores gained idempotent delete and a failed
  secret-free row commit compensates the preceding material write.
- Memory CAS/rename (4): the existing atomic adapter operations are now modeled
  explicitly: one CAS winner per generation, stale-write rejection, generation
  stability for idempotent writes, and identity-preserving replace-rename.
- Worker drain (2): claim admission and drain acknowledgement share a generation
  gate. `begin_drain` linearizes against the short claim section, so no claim can
  begin after it returns.
- Config activation (2): publications carry their authoring generation; SQLite
  and PostgreSQL compare that generation in the publication transaction, and the
  live catalog never replaces a newer installed generation with an older one.
- Schema migration (3): bundles must be dense from version one; the production
  step kernel cannot roll back or skip, and replay of an applied plan is a no-op.

### Existing architecture exposed as proof kernels/protocol models

- Sandbox admission (3): isolation floors and requested capabilities are total,
  and fail-closed policy cannot authorize a downgrade.
- Consent state (3): any withdrawal vetoes full capture, purpose upsert retains
  one row, and erasure withdrawal is absorbing/idempotent.
- Credential-pool selection (4): disabled, cooling, and exhausted members cannot
  be selected; an empty eligible set fails closed.
- Cross-protocol tool results (3): only the matching call is accepted, each call
  is consumed at most once, and terminal runs reject late results.
- Audit ordering (2): a management business action follows its audit call and the
  call id is stable across the modeled retry boundary.

### Deliberately narrower guarantees

Three boundaries remain narrower than a cross-process atomicity claim:

- Session persistence and webhook enqueue use different repositories. Once
  `emit_fact` runs, the outbox row is durable and retryable, but a process crash
  between the session commit and that call is still a gap. Closing it requires
  placing the lifecycle outbox in the session repository transaction.
- Credential compensation is guaranteed when `repo.put` returns an error. A hard
  process crash after the secret write but before compensation still requires a
  durable creation-intent journal or a same-database transaction.
- `AuditCommit.tla` verifies audit-before-business ordering and stable identity.
  The default sink is structured tracing, not a database transaction shared with
  every management store. End-to-end durable audit/business atomicity therefore
  requires a durable audit-intent store or per-domain transactional outbox.

These limitations are not counted as eventual-delivery or exactly-once claims.
The ledger names only the safety boundary actually linked to production code.
