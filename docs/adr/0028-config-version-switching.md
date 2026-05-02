# ADR-0028: Configuration Version Switching

- **Status**: 📐 Proposed
- **Date**: 2026-05-02
- **Depends on**: ADR-0023, ADR-0026

## Context

ADR-0026 records every admin write to a queryable audit log with full
`before` / `after` snapshots. Recording change history is necessary but not
sufficient: when an operator identifies a bad change in the log, they still
have to manually copy the old payload and re-`PUT` it, hoping they did not
fat-finger anything in the process. There is no first-class "roll this
resource back to that version" operation.

The need shows up in three concrete situations:

1. **Bad config push.** An agent's `system_prompt` was rewritten badly by a
   well-meaning user. The previous-good prompt is sitting in the audit log,
   but rolling back means manually round-tripping JSON through the editor.
2. **A/B comparison.** An operator wants to read the spec from two weeks ago
   side-by-side with today's spec to understand what behavior drift came from
   what change.
3. **Compliance reproducibility.** "Show me the exact agent spec that
   produced run X" is a common forensics question. The audit log has the
   answer but no API to materialise a single past version.

The audit log alone covers question 3 (operators can read raw events), but
not 1 or 2. ADR-0028 closes that gap by adding a small restore API on top of
ADR-0026's storage.

This ADR is independent of ADR-0025 (draft / publish lifecycle). Version
switching restores **already-published** state from the audit log; ADR-0025
addresses **un-published edits**. The two are complementary: when ADR-0025
is implemented, restoring a version writes to the draft slot first and then
publishes, so the rollback itself is reviewable.

## Decisions

### D1: Version identity is the audit event ULID

Each `AuditEvent` introduced by ADR-0026 carries a ULID `id`. That ULID
serves as the version identifier exposed by this ADR. No separate version
counter is introduced.

Rationale: ULIDs are time-ordered and globally unique; a version reference
of `01HXXXX...` always picks out exactly one historical snapshot. Adding a
parallel `revision: u64` would require a secondary index for time order and
would not survive future log compaction.

### D2: Restore is a write, not a mutation of the past

```
POST /v1/config/:namespace/:id/restore
Body: { "version": "<audit-event-ulid>" }
```

Restoring a resource to version V is implemented by:

1. Fetching the audit event with `id = V` and `resource = <namespace>/<id>`.
2. Selecting the payload to write (see D3).
3. Calling `ConfigService::update(namespace, id, payload)` (or `create` when
   the resource has been deleted).
4. Emitting a new audit event with `action = AuditAction::Restore`,
   `resource = <namespace>/<id>`, `before = <current spec or null>`,
   `after = <restored spec>`, and an additional `restored_from = V` field.

The original event V is **never modified**. The audit log remains an
append-only history. Restoring twice produces two new restore events.

This matters because it preserves the invariant that the log is a complete
record. There is no "undo": rolling back creates a new history entry that
points to V.

### D3: Payload selection rules

Given an audit event V for resource `<ns>/<id>`:

| `V.action` | Payload that gets restored |
|------------|----------------------------|
| `Create`   | `V.after` |
| `Update`   | `V.after` (the post-update state) |
| `Delete`   | `V.before` (the spec right before deletion); routed to `create()` because the resource currently does not exist |
| `Restart`  | Refuses with 422 — runtime control events have no spec to restore |
| `Publish`  | `V.after` (the published state — see ADR-0025) |
| `Restore`  | `V.after` (allows chaining: restore the result of a previous restore) |

The default payload is what the operator usually means by "go back to how it
looked at point V". A future query parameter `?point=before` may pick
`V.before` instead — useful when the goal is "undo this specific Update,
which made things worse".

### D4: Restore goes through normal validate + apply

The restore handler does not bypass any of the five validation layers
described in ADR-0026's context. The fetched payload is treated as a
fresh `PUT` body, with one exception: the `created_at` timestamp on the
restored payload is preserved from the original record (not re-stamped).
`updated_at` is set to the restore time.

Two concrete consequences:

- **Reference drift fails the restore.** Restoring an `agents/coder` spec
  whose `model_id` references a model that has since been deleted will fail
  at the resolver, return `422 Unprocessable Entity` with the structured
  resolver error, and emit no new audit event. The operator must restore the
  model first, change the `model_id`, or delete the dangling reference.
- **Schema evolution that is not additive can fail.** Restoring a payload
  written under an older schema version that has since received a breaking
  rename will fail at deserialization. Schema changes today are
  additive-only (`#[serde(default)]` on every new field), so this is a
  forward concern rather than a current bug.

### D5: Version listing reuses the audit log endpoint

No new "list versions" endpoint is introduced. The existing audit log query
(ADR-0026, D3) already supports `?resource=agents/coder`, returning the
chronological history of that resource. Clients that want a per-resource
view call the audit endpoint with the resource filter; the admin console UI
calls it from the editor's history tab.

This keeps the API surface narrow. A convenience wrapper
`GET /v1/config/:namespace/:id/versions` may be added later if multi-call
chattiness becomes an issue, but the underlying data is identical.

### D6: Diff is computed client-side

No diff endpoint is introduced. The admin console computes diffs in the
browser from any two audit events' payloads. JSON diff is a solved problem
in JavaScript (`fast-json-patch`, `microdiff`, etc.), and keeping it on the
client lets the diff renderer evolve without server changes.

The server commits to one thing here: any two audit events for the same
resource carry full payloads (not deltas). This is already true under
ADR-0026 D1; this ADR only relies on it.

### D7: Retention coupling

The window in which a version is restorable equals the audit log retention
window (ADR-0026 D4 — default 90 days). Once an event is pruned, the
version it represents can no longer be restored. This is documented and not
worked around: deployments that need longer rollback windows configure a
larger `audit_retention_days`, accepting the storage cost.

The restore endpoint returns `404 Not Found` with a body that distinguishes
"version never existed" from "version expired by retention":

```json
{ "error": "version not found", "reason": "expired" }
{ "error": "version not found", "reason": "unknown" }
```

This signal helps operators distinguish "I typed the ULID wrong" from
"that version is gone, nothing I can do".

### D8: New `Restore` audit action

`AuditAction` (defined in ADR-0026 D1) gains a new variant:

```rust
pub enum AuditAction {
    Create,
    Update,
    Delete,
    Restart,
    Publish,   // ADR-0025
    Restore,   // ADR-0028
}
```

`Restore` events carry an additional optional field on `AuditEvent`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub restored_from: Option<String>,  // the source version ULID
```

`Update` and `Create` events leave `restored_from` as `None`. Older audit
records produced before this ADR ships continue to deserialize cleanly
because the field is `#[serde(default)]`.

### D9: Admin console integration

The agent editor (and analogously the model / provider / mcp-server editors)
gains a **History** tab alongside Basics / Tools / Plugins / Delegates /
Advanced. The tab:

- Calls `GET /v1/audit-log?resource=<ns>/<id>` for the version list
- Renders one row per event: timestamp, actor, action, summary diff
- Per-row "View" expands a payload viewer
- Per-row "Restore to this version" opens a confirm dialog with a
  side-by-side diff (`current ↔ this version`); on confirm calls the
  restore endpoint and updates the editor with the new spec

The Dashboard "Recent admin activity" widget (ADR-0026 consequence) gains a
small Restore icon next to events whose source resource still exists.

The cross-link from a delete event to "Restore" is the highest-leverage
piece: the most common rollback request is "I just deleted this by mistake".

## Alternatives Considered

**Snapshot-based versioning separate from the audit log.** Cleanest model
on paper: every write produces both an audit event and a copy under
`<ns>/<id>@<rev>`. Rejected because it doubles storage, doubles write IO,
and creates two sources of truth that can drift if the audit write fails.
The audit log already carries the snapshot; restoring from it is one extra
read.

**Git-style branches.** Treat each spec as a branchable artifact with merge
operations. Rejected as out of scope: branching introduces conflict
resolution semantics that have no obvious meaning for a typed spec, and
would require a UX much heavier than the current admin console can
absorb. Operators today edit linearly; making them think about branches
would regress usability.

**Soft-delete with tombstones.** Mark deleted records as
`{ deleted: true }` instead of removing them, and treat undeletion as a
restore. Rejected because it tangles the type system: every consumer of a
spec would need to know about the tombstone state, and stores would have
to filter it out of every list. The audit log handles deletion history
without polluting the live store.

**Add a dedicated `version` query parameter to `GET /v1/config/...`.** E.g.
`GET /v1/config/agents/coder?version=01HXXX` returns the historical
payload. Rejected because it duplicates the audit log query. The
information is identical; adding a second access path means two response
shapes to maintain. Operators get the historical content from
`/v1/audit-log` already.

## Consequences

- The admin console gains a History tab in every resource editor and an
  integrated rollback flow. This addresses the most common operator pain
  ("get me back to what worked yesterday").
- Storage cost is unchanged from ADR-0026: no new persistence is
  introduced, only a read+write API that consumes existing audit data.
- Restore is bounded by retention. Deployments that occasionally need
  longer rollback windows must opt into longer retention. There is no path
  to restore a version older than the configured window.
- Restore failures are loud: any apply-time failure (resolver errors,
  schema mismatch) returns `422` with the structured error and emits no
  new audit event. Partial restores are not possible; the operation is
  atomic at the single-resource level.
- Cross-resource cascade is **not** in scope. Restoring an agent does not
  also restore its model and provider. Operators who need a coordinated
  rollback across multiple resources sequence the calls themselves.
- The existence of a Restore action makes the audit log a load-bearing
  operational tool, not just a passive record. Failure modes that were
  previously "lose visibility" (audit write fails) become "lose
  reversibility". Implementers should monitor `awaken_audit_write_failures`
  metric and alert on it once this capability ships.
