# ADR-0022: Run Dispatch Data Model

- **Status**: Accepted
- **Date**: 2026-04-12
- **Depends on**: ADR-0012, ADR-0019
- **Updates**: ADR-0019

## Context

The mailbox-backed runtime now supports durable background runs, reconnectable
HITL approval, structured run outcomes, and cross-transport tracing. The earlier
model blurred several responsibilities:

- `Thread` held metadata while messages lived in a separate store.
- `RunDispatch` duplicated request messages and also acted like the run result.
- `RunDispatchStatus::Accepted` was easy to read as agent success, although it
  only meant that the queue delivery had been consumed.
- waiting runs were partly encoded through legacy termination strings.
- run IDs, mailbox dispatch IDs, and transport session IDs were hard to correlate.

The data model needs to preserve the mailbox as an optional dispatch plane while
making the runtime truth clear and compact.

## Decision

### D1: Thread is the conversation aggregate

`Thread` remains the thread-level anchor. It stores metadata plus run pointers:

- `active_run_id` points at the run currently executing.
- `open_run_id` points at the unfinished run that may be resumed.
- `latest_run_id` points at the most recent run for the thread.

The thread does not own runtime outcome, queue delivery details, or dispatch
epoch state. Dispatch epoch remains part of the orthogonal `RunDispatch` plane.

### D2: MessageRecord is the durable thread log projection

`Message` remains the protocol payload. `MessageRecord` is the durable
thread-owned projection that assigns sequence numbers and producer metadata:

```text
Thread 1 -> * MessageRecord
```

Runs read and produce messages, but they do not own message bodies. A run input
or output stores message ranges and IDs instead of another copy of the full
conversation.

### D3: RunRecord is the source of truth for user intent and outcome

`RunRecord` represents one user intent. It owns:

- identity: `run_id`, `thread_id`, `agent_id`, parent run linkage
- request snapshot: origin, sender, extras, decisions, frontend tools, and
  trigger message references
- input/output references into the thread message log
- lifecycle: `Created`, `Running`, `Waiting`, `Done`
- structured waiting state and durable waiting tickets
- structured terminal outcome: termination reason, final output, error payload
- trace IDs copied from the current activation

`RunRequestSnapshot.input_message_ids` and `input_message_count` are the request
references. The message bodies remain in the thread log.

### D4: RunDispatch is dispatch-only

`RunDispatch` is the durable queue record for one activation attempt.

```text
RunRecord 1 -> * RunDispatch
```

A dispatch owns delivery, lease, retry, and queue audit state:

- queue status: `Queued`, `Claimed`, `Acked`, `Cancelled`, `Superseded`,
  `DeadLetter`
- claim token, claimant, lease deadline
- priority, dedupe key, retry counters, last delivery error
- dispatch trace projection: `run_id`, `dispatch_instance_id`, `run_status`,
  termination, response, error

`Acked` means the dispatch was consumed and should not be retried. It does not
mean the agent succeeded. The run result must be read from `RunRecord.outcome`
or the dispatch result projection.

### D5: Waiting is an intermediate state of the same run

Approvals, user-input waits, manual pauses, rate limits, background-task waits,
and external-event waits are all modeled as `RunStatus::Waiting` with a
structured `RunWaitingState`.

Resuming a waiting run creates a new dispatch for the same `run_id`; it does not
create a new run. The thread's `open_run_id` is the durable pointer used by
control APIs to find the resumable run. `RunWaitingState.tickets` stores the
pending approval/input control points needed by reconnecting UIs.

### D6: RunIdentity is split by concern but flat on the wire

`RunIdentity` is composed of three sections:

- `RunRef`: stable run/thread lineage and agent identity
- `RunTrace`: mailbox dispatch, transport session, and request correlation
- `RunExecutionContext`: origin, run mode, and adapter kind for policy hooks

The serialized event payload stays flat. Rust callers can
depend on the smaller concept they need instead of treating every correlation
field as core run identity.

### D7: Lifecycle flow

New user intent:

1. append input messages to the thread log
2. create `RunRecord(status=Created)`
3. create `RunDispatch(status=Queued, run_id=...)`
4. claim dispatch and mark the run `Running`
5. checkpoint produced messages and run state

Waiting:

1. persist `RunRecord(status=Waiting, waiting=...)`
2. ack the current dispatch
3. clear `active_run_id`, keep `open_run_id`

Resume:

1. load `Thread.open_run_id`
2. append any new user input or decision snapshot
3. create a new dispatch for the same `run_id`
4. resume the same run

Terminal completion:

1. persist `RunRecord(status=Done, outcome=...)`
2. ack the current dispatch
3. clear `active_run_id` and `open_run_id`
4. set `latest_run_id`

## Consequences

- Queue delivery and runtime outcome are no longer conflated.
- UI and business logic should use `RunRecord.status`, `RunRecord.waiting`, and
  `RunRecord.outcome` to decide what happened.
- Operators can still inspect the latest dispatch projection on `RunDispatch`
  without turning queue status into business status.
- Reconnectable HITL flows can recover pending tickets from durable waiting
  state.
- Sub-agent result selection remains a thread-message concern: consumers should
  inspect the child run's produced message range and select the final
  non-tool assistant message.

## Compatibility Notes

- `RunDispatch` replaced the old mailbox dispatch type name.
- `RunDispatchStatus::Acked` replaced the old accepted-delivery wording.
- `RunDispatchResult` is the runtime-result projection stored on a dispatch.
- Dispatch records no longer store request message bodies or activation
  payload. Reconstruction loads `RunRecord.request` and the thread message log.
