# ADR-0094: Session Access Is One Typed Verdict; Run Admission Outcomes Are Classified

- Status: Accepted
- Date: 2026-10-25
- Amended by: ADR-0102 (D3 splits determinate not-admitted from undecided)

## Context

The Session execution acceptance checklist recorded four product gaps that
share two causes.

1. **Ownership was decided in several places, and its result was not carried.**
   The Managed route checked the owner, `application_guard` resolved the
   application binding, and some application entrypoints checked again while
   others (the Event append) trusted their caller. Each check returned a
   different answer, and a check that passed handed on only a bare
   `session_id`. One owner-scoped admission also combined *authorize* and
   *create*: an unknown id created a Session, a foreign id answered
   "not found", so the two were distinguishable.
2. **A Run admission refusal had no durable classification.** The Run
   reservation precedes the Session activity CAS. When the CAS refused or
   failed, nothing recorded which, so reservation repair (`RecoverOrAdmit`)
   later admitted a Run whose caller had been told it was refused.

The application grant already makes the public application protocols safe:
`application_guard` maps every client thread alias through a server-issued
binding to an existing Managed Session and answers an unbound alias with the
same 403 whether or not a Session exists. The gaps are in the in-process
application API behind it.

## Decision

### D1 `SessionAccess` is the only owner-scoped input

`awaken-session-application` defines `SessionAccess { owner_scope, session_id }`
with private fields. It is minted only by `SessionRootReads::authorize`, which
reads the durable owner once:

| Durable owner | Verdict |
|---|---|
| equals the caller | `SessionAccess` |
| another Workspace | `NotFound` |
| none | `NotFound`, byte-equal to the foreign verdict |
| read fails | `Unavailable` |

Every owner-scoped command reachable outside the crate takes `&SessionAccess`:
`append_owned_session_event_batch` and its append-and-drive form (and the
`SessionEventBatches` role), `admit_owned_session_run`, `run_owned_session`,
`admit_profiled_session_run`, `resume_owned_session_run` and its streaming
form. The unscoped Event append is `pub(crate)` and serves only this crate's
reconcilers. Callers mint access at their trust boundary: the Managed route
from its authenticated Workspace, the owner-scoped Run application from the
Workspace `application_guard` stamped, and Coordinator jobs from the Workspace
of the job they own.

`close_session_event_admission` keeps `owner_scope` plus `session_id` because
it must also answer for a deleted Session from its tombstone; its live branch
uses the same `authorize` verdict.

### D2 Creation is explicit; the owner path never creates

`admit_owned_session_run` never creates a Session: a Session deleted after the
verdict answers like any missing Session. Only the single-tenant Run
application, which derives its own Workspace from the thread, keeps the
first-Run creation (`admit_or_create_session_run`, crate-private). The
application binding is the namespace of client thread aliases; no second
per-owner id scheme is introduced.

### D3 Run admission outcomes are classified

After the Run reservation is durable, the Session activity admission has four
outcomes (amended by ADR-0102 D2):

| Refusal | Durable effect | Caller answer | Reservation repair |
|---|---|---|---|
| budget boundary (definitive) | rejection receipt `managed:session-run-rejection:` in the root's idempotency index | `budget_reached` | `Rejected`, replayed by the caller's retry and cancellation alike |
| Session store refused before applying (`NotApplied`, determinate) | none | `session_run_not_admitted` (retry the same operation) | completes the admission exactly once, or deletes it on cancellation |
| Session store outage with unknown effect, or CAS conflict (undecided) | none | `session_run_admission_pending` (retry the same operation) | completes the admission exactly once, or deletes it on cancellation |

| other admission rule | none | its own class | its own class |

An undecided outcome is not a failure: the reservation repair owns its
settlement, and the same operation id is the only safe retry. A store reports
`NotApplied` only when it can prove nothing was applied; any doubt stays
`AuthorityUnavailable`, which is undecided. Not admitted still means the same
operation id, because repair may complete the durable reservation. A rejection
receipt is written only when the store is healthy; if it cannot be written the
refusal is reported as pending, which repair still settles.

Both SQL adapters now report the existing `NotApplied` class when a failed
write has not invoked COMMIT. A COMMIT driver failure retains the existing
`AuthorityUnavailable` class. This describes one attempt, never proves an
earlier delivery absent and never authorizes quota release. The Managed error
envelope retains the existing stable Run code alongside status, type and message.

### D4 Aggregate admission applies the protocol's Thread rules

Every new Event batch passes `admit_event_batch_topology` before the root CAS:
an Interrupt names only a coordinated, still-interruptible child, and a tool
reply names the exact committed answerable ticket (current Awaiting Run,
correlation and call) of its target Thread. A new reply must carry its Thread
version and Awaiting commit; only retained legacy rows may lack them, and the
delivery fence keeps the version check.

A pure Interrupt on a budget-paused, inactive root is an accepted no-op only
after committed root/child snapshots prove no live Run and the existing full
prefix validator proves those observations remained current. Unknown or moving
sources cannot supply that result. This policy lives in the Event application;
the protocol only lowers targets and projects its result.

Every Event command uses the same canonical identity in the original Session
idempotency index. Its receipt distinguishes a retained Event batch from an
accepted no-op. Both compete with final close through the same transaction and
unique key. A no-op inserts only its receipt at the observed root revision;
it changes no root, timestamp, activity, outbox, dispatch or Run. Exact replay
returns that receipt before current budget policy, including after allowance
resumes. PostgreSQL rechecks the same receipt after acquiring its root lock
because concurrent no-ops deliberately do not advance the root revision.

The application result and Native `event_noop` close proof contain no batch or
executable event identity. Deletion retains the explicit known-empty no-op
proof through the existing owner-bound tombstone codec; legacy missing evidence
remains unknown. A no-op receipt cannot certify a root mutation or cleanup.

The new receipt-kind column is appended through the existing schema ledger;
historical rows default to their original root-mutation meaning. Event writers
must upgrade as one source/image cohort: mixed old/new Event writers are not a
supported deployment because old writers do not use the canonical Event key.

## Consequences

- A command cannot reach a Session its caller does not own without first
  holding a verdict, and the compiler enforces it outside the crate.
- Foreign and missing Sessions are one answer at the single place that decides.
- A refused Run is never admitted later; an undecided one is admitted at most
  once.
- Rejection receipts advance the root revision without a domain fact.
- Protocol adapters mint access instead of comparing owners themselves.

## Verification

The acceptance ledger binds the executed tests of T06-C01, T07-C01, T11-C02 and
T13-C03 in `docs/design/session-execution-test-acceptance/`.
