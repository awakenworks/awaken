# Model runtime dialect pinning cause-effect test design

## Scope

The immutable publication carries both adapter family and exact API dialect.
Execution validates the pair before constructing a credential-backed executor.
OpenAI Responses must never silently execute through Chat Completions merely
because both belong to the OpenAI adapter family.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | Modern publication pins a supported matching dialect/adapter pair |
| C2 | Publication pins a known dialect with the wrong adapter family |
| C3 | Publication pins an unknown dialect |
| C4 | Publication pins OpenAI Responses |
| C5 | Legacy publication has an empty dialect and a supported adapter |

| ID | Effect |
| --- | --- |
| E1 | Matching executor family is constructed |
| E2 | Realization fails with dialect/adapter mismatch |
| E3 | Realization fails as unsupported dialect |
| E4 | Responses uses its dedicated protocol executor, not Chat Completions |
| E5 | Legacy snapshot remains readable/executable through its adapter pin |

## Decision table

| Cause/effect | T1 | T2 | T3 | T4 | T5 |
| --- | --- | --- | --- | --- | --- |
| C1 matching modern | 1 | 0 | 0 | 0 | 0 |
| C2 mismatch | 0 | 1 | 0 | 0 | 0 |
| C3 unknown | 0 | 0 | 1 | 0 | 0 |
| C4 Responses | 0 | 0 | 0 | 1 | 0 |
| C5 legacy empty | 0 | 0 | 0 | 0 | 1 |
| E1 executor | 1 | 0 | 0 | 1 | 1 |
| E2 mismatch | 0 | 1 | 0 | 0 | 0 |
| E3 unsupported | 0 | 0 | 1 | 0 | 0 |
| E4 dedicated Responses | 0 | 0 | 0 | 1 | 0 |
| E5 compatibility | 0 | 0 | 0 | 0 | 1 |

Automated evidence is in the runtime-contract wire-shape test, model publication
tests, server executor-seam tests, and provider Responses protocol tests.
