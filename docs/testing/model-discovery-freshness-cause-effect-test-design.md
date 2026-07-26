# Model discovery freshness cause-effect test design

## Scope

This slice records the freshness of successful, user-triggered provider API
discovery. Discovery remains on demand: the control plane does not make a timer
authoritative for provider inventory. A timestamp is evidence of a complete
successful observation, never evidence of a failed or partial request.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | First complete listing contains a provider-owned model at time T1 |
| C2 | Later complete listing still contains that model at T2 |
| C3 | Later complete listing omits that model at T2 |
| C4 | A discovered route is explicitly authored by a user |
| C5 | Listing contains an invalid empty model id |
| C6 | Provider transport/protocol discovery fails |
| C7 | Credential belongs to another Workspace |

| ID | Effect |
| --- | --- |
| E1 | Route is active and `last_seen_at_unix_ms = T1` |
| E2 | Route stays active and last-seen advances to T2 |
| E3 | Route becomes unavailable and retains T1 |
| E4 | Route becomes manual/active and last-seen is cleared |
| E5 | Reconciliation is rejected atomically |
| E6 | Previous catalog and freshness remain unchanged |
| E7 | Request fails closed before provider discovery |

## Cause-effect graph and constraints

```text
C1 -> E1
C1 & C2 -> E2
C1 & C3 -> E3
C1 & C4 -> E4
C1 & (C5 | C6) -> E5 & E6
C7 -> E7 & E6
```

- **M**: only a complete successful provider listing can write last-seen.
- **M**: unavailable means absent from the latest successful complete listing;
  a failed listing must not make every model unavailable.
- **R**: manual authority masks provider discovery on the same offering key.
- **R**: all rows reconciled by one successful request share its observation time.

## Decision table and test cases

| Cause/effect | T1 | T2 | T3 | T4 | T5 | T6 | T7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| C1 first listing | 1 | 1 | 1 | 1 | 1 | 1 | 0 |
| C2 present at T2 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C3 absent at T2 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C4 manual authoring | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C5 invalid listing | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C6 provider failure | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C7 wrong Workspace | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| E1 active at T1 | 1 | - | - | - | - | - | 0 |
| E2 advances to T2 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| E3 unavailable, retains T1 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| E4 manual, timestamp cleared | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| E5 atomic rejection | 0 | 0 | 0 | 0 | 1 | 1 | 0 |
| E6 catalog unchanged | 0 | 0 | 0 | 0 | 1 | 1 | 1 |
| E7 pre-discovery denial | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

Automated evidence:

- T1-T5: model-catalog repository conformance suite, executed against memory,
  SQLite, and PostgreSQL when its test database is available.
- T6-T7: admin-config discovery HTTP tests.
- Wire/UI alignment: generated-contract freshness plus web typecheck/build.
