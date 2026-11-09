# Personal Agent selection test design

The Control Config owner stores a person's selection preference separately from
Agent definitions and IAM authority. A selected Agent is usable only when the
current native policy permits use. A historical selection or receipt never
substitutes for that decision.

## Storage contract

The durable identity is the exact Workspace, native issuer, Account and Agent
id tuple. The HTTP edge derives the Account and issuer from verified native
identity, never request metadata, an Actor or a display name. A request body
contains only its original key, expected generation and desired selection.

The preference CAS and immutable original receipt commit in one Config-store
transaction. Replaying an old key returns the original receipt before
interpreting current preference state. It cannot undo a later removal. The
receipt has no manifest, definition, permission level or conversation content.

| Cause | Required result | Executable evidence |
| --- | --- | --- |
| Absent preference and generation zero | One positive generation and one original receipt | Domain decision and both adapter conformance tests |
| Stale generation or changed body under the original key | Conflict; existing state and receipt remain intact | Domain and adapter conformance tests |
| Original select replayed after a later removal | Original receipt returned; current preference remains removed | Domain and adapter conformance tests |
| Another issuer, Account, Workspace or Agent with the same request key | Separate data; no earlier state or receipt is visible | Actual adapter conformance tests |
| Concurrent requests with the same baseline | One commit, one typed generation conflict, one receipt | Independent file-backed SQLite stores and PostgreSQL transactions |
| Receipt insertion fails after the proposed state write | Neither state nor receipt commits | SQLite trigger and PostgreSQL constraint failure tests |
| Extra owner, issuer, Workspace, Actor, authority or definition fields | Body decoding refuses them | Domain wire tests |

The preference uses its own mandatory repository port, implemented by the
existing Config-store adapters. Definition repositories do not acquire optional
preference methods or unsupported capability defaults.

The production decision is in
[personal_selection.rs](../../crates/platform/config/awaken-agent-config/src/personal_selection.rs).
The adapters share the
[conformance table](../../crates/platform/config/awaken-config-store/tests/support/personal_selection.rs),
called by the actual
[SQLite](../../crates/platform/config/awaken-config-store/tests/integration.rs)
and [PostgreSQL](../../crates/platform/config/awaken-config-store/tests/postgres.rs)
tests. The additive V13 migrations belong to the existing Config schema stream;
previous published migration bodies and installed Agent definitions stay intact.

## Authority and qualification boundaries

Repository tests establish storage behavior, not Account authentication or IAM
permission. HTTP integration must separately prove direct-human management,
explicit action/resource registration, current permission and revocation,
non-disclosing manifest pagination, and denial of legacy definition-read bypasses.
Normal controlled engine execution is a separate admission path.

Owner original-outcome read is independent from current Agent use. With use
withdrawn, an admitted owner may still read a content-free receipt under its
original key. If owner read is denied, the caller retains its unknown original;
Denied is not NotFound and cannot justify a replacement key. An absent receipt
also does not authorize replacement while an earlier request may be in flight.

A deterministic negative control splits the state and receipt commits: the
receipt-failure test must then observe the leaked state and fail. Unit decisions,
real SQLite/PostgreSQL adapters, live IAM/HTTP revocation and deployed browser
journeys are distinct qualification claims. A skipped PostgreSQL fixture is not
PostgreSQL evidence.
