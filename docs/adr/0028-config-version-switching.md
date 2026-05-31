# ADR-0028: Config Version Switching

- **Status**: ✅ Accepted
- **Date**: 2026-05-01
- **Depends on**: ADR-0026

## Context

ADR-0026 introduced a structured audit log that records every admin config
change as an immutable event with `before` and `after` payloads. Operators
asked for a one-click rollback: given a version ULID from the audit log,
restore the resource to that exact state.

The existing `/v1/config/:namespace/:id` CRUD surface already covers
create/update/delete with full validate-and-apply semantics. Restore keeps a
separate editing-store boundary: it reconstructs the historical spec and writes
it back to `ConfigStore`, but it does not publish a new runtime registry
snapshot. Operators promote the restored payload with a normal config write
after review.

## Decisions

### D1: API surface

`POST /v1/config/:namespace/:id/restore` with body `{"version":"<ulid>"}`.
The ULID is the `id` of an existing audit event.

### D2: Auth

The endpoint is behind `ensure_admin_auth`, same as all other config routes.

### D3: Payload selection

| Source event action | Payload used |
|---------------------|-------------|
| Create              | `event.after` |
| Update              | `event.after` |
| Publish             | `event.after` |
| Restore             | `event.after` |
| Delete              | `event.before` (re-creates the resource) |
| Restart             | 422 — no spec payload exists |

### D4: Timestamp handling

`updated_at` is always set to the restore time (via the normal `prepare_body`
path). For resources being re-created from a Delete event, `created_at` from
the restored payload is preserved rather than being overwritten with the
current time.

### D5: Cross-resource guard

If `event.resource` does not equal `<namespace>/<id>` from the URL, the
request is rejected with 422. Cross-resource restores are not permitted.

### D6: Editing-store restore, explicit publish

Restore validates and persists the reconstructed payload, then records the
restore audit event. It deliberately does not call the runtime apply path or
replace the active registry snapshot. A restore that should become visible to
new runs must be promoted through a normal create/update/override write, which
uses the standard validate-and-apply path and records its own audit event.

### D7: Version-not-found response

A missing audit event (either never existed or pruned by retention) returns:

```json
{"error": "version not found", "reason": "unknown"}
```

The `reason` is always `"unknown"` because the server cannot distinguish a
pruned event from one that never existed. The ADR originally proposed
`"expired"` for the pruned case but defensive uniformity is preferred.

### D8: Audit event for the restore

After a successful restore, a new audit event is emitted:

```json
{
  "action":        "restore",
  "resource":      "<namespace>/<id>",
  "before":        <spec before restore or null>,
  "after":         <restored spec>,
  "restored_from": "<source event ULID>"
}
```

### D9: AuditAction enum extension

`AuditAction::Restore` is added to `awaken-contract`. The new
`restored_from: Option<String>` field on `AuditEvent` uses
`#[serde(default, skip_serializing_if)]` so all existing events
deserialise cleanly without schema migration.
