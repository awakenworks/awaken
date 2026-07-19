# Formal verification expansion: 79 additional obligations

The first batch expanded the versioned safety ledger from 28 to 56 formalizable,
machine-linked obligations. The original 28 remain unchanged. Those additional
28 are grouped below by the production boundary they verify; the second batch
below raises the total to 92; transactional hardening raises it to 97; the
production-completion batch at the end raises the final total to 107.

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
The transaction-hardening batch below now places lifecycle facts and their outbox
rows in the same session-repository transaction. The four environmental classes
remain deliberately outside the safety percentage.

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

## Transaction hardening: 5 additional obligations

- Session lifecycle/outbox (1): create commits the session row, owner and lifecycle
  fact together; archive commits its durable terminal state with the fact; delete
  commits a tombstone with the fact. Production Webhook assembly consumes this
  session-local outbox, including rows present before process restart.
- Credential creation recovery (2): a secret-free intent is durable before the
  secret write, source publication atomically retires it, and CLI startup
  plus periodic reconciliation either preserves a published source or
  idempotently removes the unpublished material.
- Durable management audit (2): the config store records a pending stable call
  before the action, then commits the draft and pending→committed transition in one
  SQLite/PostgreSQL transaction. A committed call replay is a business no-op and a
  conflicting reuse of the identity fails closed. Structured tracing is now only
  an observability projection, not the audit authority.

These changes close the three previously documented cross-process transaction
gaps. They do not claim eventual network delivery or exactly-once third-party
effects; those remain environmental.

## Production completion: 10 additional obligations

- Webhook retry (1): both legacy and session-local outboxes now have an immediate
  drain and a periodic serialized reconciliation loop. A failed delivery remains
  pending; a later tick can redeliver it without a restart or new event.
- Credential inventory (3): sealed stores enumerate opaque references; the
  reconciler preserves committed and pending-intent references, deletes only
  unreferenced `sec:cred:` material, reports referenced-but-missing material, and
  runs at startup plus every 60 seconds. Foreign keys sharing the store are fenced.
- All management HTTP mutations (3): non-read requests are body-fingerprinted and
  durably audited before the handler runs. Audit failure blocks the business
  operation; a successful response marks the intent committed. A crash between
  the two leaves a durable pending record instead of an unaudited write.
- Resource bindings (3): draft, audit completion, and a secret-free resource
  effect are committed in one config-store transaction. The effect is applied by
  an idempotent startup/30-second reconciler and retired only after durable
  read-back. SQLite and PostgreSQL implement the same protocol.

The production composition root now passes the durable SQLite/PostgreSQL admin
store as the shared `ResourceStore`; the previous unconditional in-memory store
was removed. Real child-process kill tests cover session, credential, and audit
crash windows, and restart persistence covers HTTP audit plus resource bindings.

## Environmental verification matrix

The safety ledger intentionally does not turn dependencies outside the process
into mathematical assumptions. Those boundaries are covered by executable tests
and release gates instead:

| Boundary not proved end-to-end | Executable evidence | Gate |
| --- | --- | --- |
| Network retry, timeout, 429/5xx, and response loss | `awaken-webhook/tests/webhook_e2e.rs` drives real loopback TCP/HTTP receivers and verifies stable `webhook-id` retries | `cargo test -p awaken-webhook --all-targets` |
| Third-party exactly-once side effects | Stable delivery IDs and response-loss retries let receivers deduplicate; no test claims an arbitrary receiver applies once | receiver contract/integration test plus reconciliation metrics |
| SQLite/PostgreSQL engine and process durability | config/session/credential process-kill tests, backend conformance, and live PostgreSQL suites | `scripts/ci/pg_tests.sh` plus workspace Rust tests |
| LLM semantic quality and sampling variance | repeated Admin Assistant golden cases score persisted, compiled configuration rather than prose alone | `python3 scripts/assistant-eval.py 3 --gate` against a live configured server |
| HTTP/MCP wire compatibility | one shared Streamable HTTP suite drives both the framework-free kernel and the real Axum router; raw and round-trip suites cover auth/session state | MCP core, testkit, and protocol crate tests |
| Middleware/composition order | management guard stamps authenticated scope into the inner durable audit layer; rejection, replay, conflict, and body-bound tests drive assembled Axum routers | `cargo test -p awaken-control --all-targets` and CLI management tests |
| Encryption implementation and key operations | sealed-secret restart, wrong-key, SQL tamper, nonce, and hard-cutover rotation tests | `cargo test -p awaken-credential-vault --all-targets` |
| Kernel/container isolation | local, Docker, Podman, and Kubernetes capability/egress/resource-limit suites exercise the actual OS boundary when available | `scripts/e2e/sandbox_capability_suite.sh` |
| Wall clocks and timers | time is supplied to pure lease/queue/cooldown kernels; backward movement, expiry edges, timeout, and reaping are property-tested | workspace Rust tests |
| Scheduler concurrency and capacity | optimistic-CAS/lease properties, multi-protocol concurrency, bounded queues, stress and soak workloads | `npm --prefix e2e run test:k6:stress` and `npm --prefix e2e run test:soak` |
| Telemetry collectors and retention operations | collector-free trace files, fake OTLP receivers, live Jaeger/Phoenix fan-out, secret scans, and bounded shutdown cover failure/flush paths | `npm --prefix e2e run test:trace` plus deployment retention drills |
| Client SDK and deployment wiring | official Managed, AI SDK, AG-UI, and A2A clients drive the served binary; worker composition and k3d suites cover role/startup/failover wiring | `npm --prefix e2e run test:protocols` and deployment E2E jobs |

The rows for arbitrary third-party exactly-once behavior, model meaning, database
implementation correctness, kernel isolation, collector retention, and cloud
control planes can only increase empirical confidence. They cannot be promoted to
formal guarantees without replacing the external component with a modeled,
transaction-participating capability.
