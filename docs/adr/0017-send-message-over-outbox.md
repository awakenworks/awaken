# ADR-0017: Agent Messaging Is Owned by Thread State, Not the Dispatch Outbox

- Status: Accepted
- Date: 2026-06-30
- Revised: 2026-09-01
- Depends on: ADR-0007, ADR-0013, ADR-0021

## Context

The former generic server-side message adapter read a target Thread's current
`ResumeTicket` and then staged a separately durable `PendingInput` in the
Dispatch outbox. Managed coordination also reused that infrastructure for an
Agent follow-up even though it already owned a deterministic fresh Run.

Those two operations do not share an aggregate or transaction. The Thread may
resume, end, or advance to another Run between the read and the stage. A later
Worker correctly refuses the stale ticket, but the already-bound message can be
stranded instead of becoming input for the Thread's next Run. Dispatch cannot
repair this by deciding which Run is awaiting: Run and ticket truth belongs to
the committed Thread.

Static composition review found no production installation of the generic
adapter. Managed coordination exposes the single `send_message` command. Its
source Thread already persists the call, execution state, recovery policy, and
result in `ActiveToolBatch` through `ThreadCommit.state`; adding a Dispatch
outbox record duplicates that durable request authority.

## Decision

### D1: The source Thread owns the Agent-message request

`send_message` remains an ordinary durable tool operation. Its committed
`ActiveToolBatch` is the request/recovery authority and its committed tool result
is the acceptance receipt. Replay uses the existing deterministic operation and
child-Run identities. There is no Agent-message table, relationship registry,
or Dispatch outbox intent.

That unsupported read-then-stage adapter is removed. Dispatch exposes no
`awaiting_run` query and never selects a target Run from a Thread status cache.

### D2: The target fresh Run owns the accepted message

Spawn and follow-up freeze exactly one user message in the target
`RunActivation.input` and enter through `enqueue_session_child`. A terminal
child's report likewise freezes one user message in a deterministic root
`RunActivation.input`, but enters through the root `enqueue` admission. Backend
Run-id idempotency rejects a same-id/different-payload collision. When the
Worker commits, the message becomes ordinary target-Thread transcript truth
through `ThreadCommit.messages`.

Dispatch therefore owns only delivery, claim, lease, retry, and settlement of
the already-frozen Run. It does not own Agent-message acceptance and does not
write a parallel `PendingInput`.

### D3: Keep external pending input separate

ADR-0021's unbound pending-input representation remains an ingress primitive for
input already accepted by an external application before a Run exists. It is
not the internal Agent messaging mechanism. The sole model-visible coordination
command is the Thread-scoped `send_message`; no generic server sender or
read-then-outbox compatibility path remains.

### D4: Refine every command from committed source truth

An authenticated Worker claim proves which Worker currently executes the source
Run; it does not authorize that Worker to change a model-emitted call. Before
Session policy or activity admission, the Runtime reads the source Thread's one
recovery prefix and requires the command's Run, call, derived operation, tool
id, normalized target, and message to match an Executing `send_message` entry in
`ActiveToolBatch`. Missing, stale, completed, or payload-mutated calls fail
closed. The validator reuses the builtin tool's `SendMessageArgs` normalizer and
does not create a fingerprint table or command registry.

A follow-up is a deterministic fresh Run queued on the existing logical Thread.
This remains true while the previous Run is Running or Awaiting: the follow-up
does not inject live input and never consumes that Run's `ResumeTicket`.

## Consequences

- Internal Agent messaging has one durable request owner: source Thread state.
- The target message has one content owner: the deterministic fresh activation,
  then the target Thread transcript after commit.
- Spawn and follow-up use one admission path and one idempotency boundary.
- Child-to-parent reports use that same activation-input boundary and create no
  root Inbox or Outbox record.
- A Managed primary Session with a published delegate or Advisor projects
  exactly the `list_agents` and `send_message` coordination subset; Managed
  children receive neither coordination nor native delegation/Advisor
  capability.
- Dispatch cannot resume an unrelated Awaiting Run or strand input on a stale
  ticket because it never performs that classification.
- External pending input and internal Agent coordination remain distinct
  concepts instead of sharing storage merely because both contain text.
- Worker authentication and committed model intent are separate checks; both
  must pass before admission.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G1/G13 committed truth and one authority.
- ADR-0007 — builtin-tool extension ports.
- ADR-0013 — outbox and pending-input delivery.
- ADR-0021 — unbound Thread inbox semantics.
