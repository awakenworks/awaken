# ADR-0107: Exact Run Command Recovery

- Status: Accepted
- Date: 2026-11-16

## Context

A caller can lose a reply or cancellation response after Native accepted the
operation. Session reuse can advance to another Run before recovery. Retrying a
Session-wide interruption could then cancel the later Run. An Event batch's
admission receipt and terminal processed markers also lack proof that each reply
committed its original effect.

## Decision

The existing committed Thread ResumeApplied classifier owns reply confirmation.
Historical profiled roots with a missing mutation-policy field retain their
Managed decoder default. Exact Run reads, replies and cancellation use the
original Workspace/private ownership boundary and existing Run fences. Fresh
profiled Run admission retains its existing profiled-mode requirement.
The Session application's read role checks the original owner and complete
command identity, including Run, original version, occurrence, correlation, call
and complete payload. It returns a positive observation only for the classifier's
exact committed receipt. The query performs no reply admission, staging,
execution, activity transfer or Session root mutation.

The Managed adapter's receipt query accepts the original reply DTO. Its batch
query accepts the original full events DTO and original idempotency and
attribution headers, then reuses the existing request fingerprint and retained
Event batch lookup. Each retained ToolReply must independently have its exact
committed receipt. Admission, terminal processing, an empty batch or another
Event family supplies no positive reply proof. Existing immutable root and
Thread receipts retain their original ownership and persistence.

Exact Run cancellation names both Session and Run. Native verifies the dispatch
row's immutable primary Session affinity and invokes the existing durable Run
cancellation owner with that Run ID. Session reuse leaves this target fixed.
Existing exact Run reads own recovery of an Ended Cancelled boundary. A
cancellation response records the existing accepted cancellation semantics.

The native query routes use POST to carry the original complete typed request.
Their handlers select only the read capabilities. Canonical compositions connect
these capabilities explicitly. Providers that lack the capability return the
existing typed unavailable result.

## Role Catalog

| Role | Existing owner | Boundary |
|---|---|---|
| ProfiledSessionReplies | SessionCoordination | Original reply admission, committed receipt observation and exact Run cancellation under Session ownership |
| SessionEventBatches | SessionEventBatchCommands | Original full-request batch lookup and complete per-reply confirmation |
| SessionCoordinationReads | CoordinatedThreads | The unique committed ResumeApplied classifier |
| StreamingRuns | WorkerPlacedSessions or SessionRuns | Original exact-affinity durable Run cancellation |

## Consequences

Application consumers can preserve an uncertain command and query its original
durable outcome across response loss, restart and Session reuse. The exact reply
classifier and dispatch cancellation owner remain authoritative. Existing
Workspace and private Session ownership checks apply before these surfaces.

The Session application coordinates the existing receipt persistence, Thread
reads and Run dispatch owners. Process composition supplies their established
pools and lifecycle.

## Validation

- The original executor reply fixture reads Absent before ResumeApplied, reads
  the exact receipt after the ticket is consumed, rejects changed complete
  identity axes and preserves the Inbox during queries.
- The application batch owner checks partial and complete original replies,
  changed payload and attribution identities, terminal processed markers and
  read-only Session/dispatch effects.
- The canonical Worker-placed cancellation owner checks foreign affinity,
  original R1 cancellation while R2 shares the Session, and exact retry.
- Native adapter contract checks preserve the original typed request and route
  identities. Public API, owner suites and integration gates qualify the actual
  final source cohort through the existing runners.

The concrete owners are the executor consumed-ticket fixture, the application
batch and legacy-root matrix, the Worker-placed exact cancellation test and the
Managed extension route contract tests. Their source paths are retained here
so the normal runners can select the actual owners.

- [Executor reply receipt owner](../../crates/platform/execution/awaken-session-executor/src/host/tests/resume_and_replies.rs)
- [Application batch and legacy owner](../../crates/platform/session/awaken-session-application/src/tests/event_batches/reply_receipts.rs)
- [Exact cancellation owner](../../crates/platform/execution/awaken-session-run-delivery/src/delivery.rs)
- [Managed route contract owner](../../crates/platform/protocols/awaken-protocol-managed/src/awaken_extensions/profiled_run_events/receipt_tests.rs)

Qualification receipts record their actual source, profile and result. This
record describes the contract and does not replace executable evidence.
