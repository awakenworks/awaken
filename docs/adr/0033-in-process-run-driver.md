# ADR-0033: The In-Process Run Driver

- Status: Accepted
- Date: 2026-07-01
- Depends on: ADR-0010 (resume), ADR-0030 (permission axis), ADR-0032 (RunnableConfig)

## Context

The runtime exposes two execution primitives — `execute` (one run to an await or
end) and `resume` (wake an aawaiting run) — plus `run`, a single-shot sugar over
`execute`. That was enough for the happy path, but any agent that **gates tools**
(ADR-0030: `Ask` awaits the run on a `ResumeTicket`) fell off a cliff: `run`
returned `RunState::Awaiting` and stopped, without even the run id needed to resume.
Callers were forced back to the primitives and hand-wrote the whole
`execute → (await → decide → resume)* → end` loop — generating ids, building
activations, registering the snapshot for by-id resume, and assembling
`ResumeCommand`s field-by-field.

The evidence that this belonged in the runtime, not each caller: the
ticket→`ResumeCommand` assembly was **duplicated** in the durable worker and the
coding-agent example, and the id generation existed twice. The orchestration had
leaked out of the execution domain into every application.

## Decision

### D1: A aawaiting run is a question; a resume is its answer

The three types have one job each and do not overlap:

- `ResumeTicket` — *what* the run is waiting for, plus the resume's identity
  (correlation, run/thread, snapshot, fingerprint). Committed by the runtime.
- `ResumeResult` — the *answer* (allow/deny, an input, a tool result). The caller
  supplies it; the ticket never contains it.
- `now_ms` — the injected clock that enforces `ticket.deadline_ms` (clock at the
  edge, so the core stays deterministic).

`ResumeCommand::from_ticket(ticket, result, now_ms)` builds the command: identity
from the ticket, answer and clock from the caller. Both the durable worker and the
in-process driver use it — one source of the resume's identity, no re-assembly.

### D2: `run_to_completion` owns the resume loop; the decision is a port

`Runtime::run_to_completion(config, thread, input, ctx, decide)` drives a run to a
terminal state, calling `decide(&ticket) -> ResumeResult` each time it awaits. It
installs the catalog and registers the snapshot (so resume resolves it by id),
generates the ids, and runs the loop. Callers never touch activations, ids, or
resume commands.

`decide` is the **in-process twin of the durable queue's out-of-band decision
delivery**: the same `ResumeResult` protocol, driven either by a synchronous
closure or by the dispatch queue across a process boundary. `run` stays as the
zero-ceremony single-shot (fresh thread, no decisions).

### D3: Delete the dead alternative

`RunWithSnapshotExecutor` / `RunWithSnapshotCommand` were defined and exported but
never implemented or constructed — a second, phantom "run a snapshot" surface.
Removed; `execute` consuming a `RunActivation` (with resolvers handling inline/by-id
snapshots) is the one real path.

## Consequences

- "Drive a run to completion, resolving decisions" is a runtime capability again;
  application objects (the coding-agent `CodingSession`) hold only session state
  (config, store, thread) and call `run_to_completion` — the resume loop, id
  generation, and `register_snapshot` all leave the app.
- No duplication: `from_ticket` replaces two hand-written assemblers; ids are
  generated in one place.
- The execution surface is a small, layered set: primitives `execute`/`resume`
  (durable), sugar `run`/`run_to_completion` (in-process). The phantom surface is
  gone.

## References

- [INVARIANTS.md](../INVARIANTS.md) — G5/G28 (resume validation), G6.
- ADR-0030 — the permission axis whose `Ask` awaits the run this drives.
- ADR-0010 — resume; this drives its loop in-process.
