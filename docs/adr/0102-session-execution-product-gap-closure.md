# ADR-0102: Pre-Root Creation Intents, Determinate Admission Refusal and Bounded Execution Limits

- Status: Accepted
- Date: 2026-10-31
- Amends: ADR-0063 (Session-owned Repository participants), ADR-0094 D3

## Context

The Session execution acceptance ledger recorded ten product gaps: each with a
reproducer showing the product did not do what the spec requires. Four of them
(T04-C03, T17-C01, T17-C02, T17-C03) share one cause. A Session-owned
Repository is prepared in the Resource Registry before its Session root exists,
and until then it was known only to the call that prepared it. ADR-0063 says an
unadopted participant is "preserved for reconciliation", but no reconciliation
could find a participant without a root. Compensation failure was only logged.
Compensation read the root, then retired the participant, with no fence against
a concurrent create that adopted it in between.

The other six were local. T33-C02: ADR-0094 D3 deliberately did not separate
"not admitted" from "unknown". T12-C03: the budget-pause interrupt no-op lived
only in the protocol parser. T06-C02: the File read sampled the lease clock
once, before its lock wait. T38-C02: `DispatchPool::shutdown` had no deadline
and `LiveInbox` was unbounded. T38-C03: disk full was a generic rejection, pool
admission waited 24 h, and Dream reported memory capacity as `internal_error`.
T23-C03: a Dream snapshot was visible before its clone finished, and cleanup
discarded its errors.

## Decision

### D1 A prepared participant is recorded by a durable creation intent

Before the external prepare, `SessionRepositoryParticipants::configure` records
the participant under its Session's creation intent: one row per Session id in
the Session store (`managed_session_creation_intent`). The row holds the owner
scope, the prepared participant ids, a phase (`preparing` or `aborting`) and a
lease (`CREATION_INTENT_LEASE_MS`, 10 minutes).

- The root insert consumes a `preparing` intent in its own transaction. An
  `aborting` intent refuses the insert as `Conflict(Tombstoned)`, because that
  identity is being retired. An intent therefore exists only while no root
  exists. A prepare against an existing root records nothing; that root's
  references decide adoption.
- Compensation claims the intent (`preparing` to `aborting`) after the purge is
  scheduled and before the definition is retired. If no intent remains and the
  root now references the participant, the concurrent create won and nothing
  is retired. This is the fence ADR-0063 lacked.
- A compensation that cannot finish leaves the intent in place. Each recovery
  cycle drives every `aborting` intent, and every `preparing` intent whose
  lease expired, without a Session root and without the original caller. It
  claims the intent, retires each participant through the existing owner-
  marker check, then retires the intent.

The intent lives in the Session store, not the Registry, so adoption and abort
are decided by one transaction in the store that owns the root.

Limitation: an owned credential that a Vault ingress created during a root-less
prepare is not named by the intent. Its retirement still follows the
participant's definition and terminal reconciliation, as before.

#### Amendment 2026-11-02: retain the completed local abort fence

Completing a claimed abort retains the same creation-intent row as `aborted`.
It is no longer pending recovery work, but it permanently refuses both a late
Root insert and a new prepare under that retired Session identity. Replaying
completion or claiming the already completed abort cannot reopen the fence.
An unrelated fresh Session identity remains admissible.

Both SQL adapters use the existing shared intent codec and Core phase predicate;
the existing creation transaction checks the retained phase before committing.
The migration appends a portable table replacement preserving all published
migration checksums, original participant ids, scope and lease coordinates.

This retained row proves local Root-adoption closure only. It does not certify
Registry/Vault closure or repair the credential-origin limitation above; those
participants still require their own durable ownership and late-write fences.

### D2 A determinate refusal is reported as not admitted (amends ADR-0094 D3)

`SessionRepositoryError::NotApplied` means the store refused before applying
anything. It flows through `SessionMutationError::NotApplied` and
`SessionActivityError::NotApplied` to `session_run_not_admitted`. Unknown
effects (`AuthorityUnavailable`) and CAS conflicts stay
`session_run_admission_pending`. Both still require retrying the same operation
id, because repair may complete the durable reservation. A store reports
`NotApplied` only when it can prove nothing was applied.

### D3 The budget-pause interrupt no-op is a root-admission rule

The original decision returned `None` and retained an early protocol answer.
[ADR-0094 D4](0094-session-access-and-run-admission-outcomes.md#d4-aggregate-admission-applies-the-protocols-thread-rules)
supersedes that representation: the application alone validates the stable
committed prefix and returns an explicit no-op with its durable command receipt.
The protocol only projects the result; there is no parser-side acceptance path.

### D4 Revocation is rechecked after every wait that can outlive it

The Worker File read re-samples the clock after the commit-epoch lock wait. It
passes the operation fence to the physical read, and re-checks the guard (or
the materialization lease) after the read. Bytes are released only while the
authority is still live.

### D5 Execution limits are bounded and classified

| Limit | Bound | Classified result |
|---|---|---|
| `DispatchPool::shutdown` | `POOL_SHUTDOWN_GRACE` (1 s) | `PoolShutdown::Abandoned { unfinished }`; stuck drains are detached, not aborted, and their leases recover the Runs |
| `LiveInbox` | `LIVE_INBOX_CAPACITY` (1024 undrained messages) | `Offer::Full` -> `LiveInboxError::Full` -> HTTP 429 `rate_limit_error` |
| SQLite pool admission | 10 s (was 24 h) | `sqlx::Error::PoolTimedOut` |
| commit store `SQLITE_FULL` | - | `coordinator::Error::CapacityExhausted` -> Unavailable `commit_capacity_exhausted` |
| Dream memory `AtCapacity` | - | the documented `memory_store_org_limit_exceeded` kind |

### D6 Dream exposes only complete snapshots and reports cleanup failure

The private input snapshot is created `Suspended` and becomes `Active` only
after every head is cloned. `release_result` attempts every step, treats a
store that was never created as retired, and returns the first failure. The
failure keeps the Dream's cleanup pending for its existing retry authority.

### D7 Budget settlements replay their original operation before cursor validation

`SessionLifecycle::reconcile_managed_budget_usage` requires the caller's
original operation id. The existing Session command receipt binds it to the
normalized input usage cursor. The lifecycle reads that receipt before testing
whether cumulative usage would rewind. An exact committed retry therefore
returns the current root without another charge, revision, reach transition or
notification, even after a later settlement or a cold reconnect. Changed input
under the same id is an idempotency mismatch; missing proof never invents a
prior admission. A receipt newer than the loaded root causes a bounded reread.

| Scope | Authoritative mechanism |
|---|---|
| Reused unchanged | Session budget arithmetic, frozen prices, neutral usage cursor, command receipt table and root/outbox transaction |
| Modified | Lifecycle settlement reads the original receipt, then commits through `SessionMutations::commit_session_snapshot_with_record`; the two coordination callers pass one retained observation id |
| Added | One pure observation-id compiler and regression cases; no new store, journal, retry service or compatibility entry point |

The coordination observation id includes the Session, source Thread, normalized
usage and already observed root runtime seconds. Unrelated title/revision writes
do not change it; new runtime time does, so a previous token receipt cannot hide
a later runtime charge. Within a call, CAS retries retain this id. After receipt
absence, the existing kernel validates monotonic usage, joins root-owned runtime
time and atomically commits the root, receipt and any reach fact. A losing exact
writer replays that same transaction and reports `reached_now = false`.
Unchanged usage without a new budget transition preserves the existing read-only
outcome. Storage outages remain errors and the caller retries the original id.

The frozen-tools title reproducer is enabled without another production path:
`compile_update_tool_settings` already preserves omitted tools on current main;
explicit authoring still uses the current compiler.

## Consequences

- The Session store gains one table (migration `next + 2` of the current tail),
  and every `SessionStore` implements `PreparedParticipantIntents`.
- A creation that outlives its intent lease may be aborted by recovery. Its root
  insert then fails as tombstoned rather than adopting a retired participant.
- `SessionEventBatches` appends distinguish a retained batch and a durably
  acknowledged no-op as defined by ADR-0094 D4.
- New public surface: `PoolShutdown`, `POOL_SHUTDOWN_GRACE`,
  `LIVE_INBOX_CAPACITY`, `Offer::Full`, `LiveInboxError::Full` and
  `coordinator::Error::CapacityExhausted`.

## Verification

The acceptance ledger binds the executed reproducers of T04-C03, T06-C02,
T12-C03, T17-C01, T17-C02, T17-C03, T23-C03, T33-C02, T38-C02 and T38-C03 in
`docs/design/session-execution-test-acceptance/`.
