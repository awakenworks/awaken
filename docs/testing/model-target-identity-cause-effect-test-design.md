# Model target identity cause-effect test design

## Scope

A catalog model name is not a routing identity. The management preview and the
immutable runtime publication must agree on one structured offering target:
`model_id + provider_id + protocol_endpoint_id`. A legacy bare `model_id` is
accepted only when it identifies exactly one active offering.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | Structured target matches exactly one active offering |
| C2 | Legacy model id matches exactly one active offering |
| C3 | Legacy model id matches multiple active offerings |
| C4 | Structured target matches no active offering |
| C5 | Request sends both `target` and legacy `model_id` |
| C6 | Request sends neither identity shape |
| C7 | One duplicate endpoint is disabled by a Profile |

| ID | Effect |
| --- | --- |
| E1 | Exact provider/endpoint/model triple resolves |
| E2 | Unique legacy input is canonicalized safely |
| E3 | Resolution fails with `model_ambiguous` |
| E4 | Resolution fails with `model_unresolved` |
| E5 | Request fails with `model_target_invalid` |
| E6 | The sole enabled offering resolves |

## Cause-effect graph and constraints

```text
C1 -> E1
C2 -> E2 & E1
C3 -> E3
C4 -> E4
(C5 | C6) -> E5
C3 & C7 -> E6
```

- **I**: a request uses exactly one of structured target or legacy model id.
- **M**: zero or multiple matches mask credential materialization; routing fails
  before any provider secret is exposed.
- **R**: structured qualifiers narrow the active-offering set; they never fall
  back to an arbitrary route.
- Provider and endpoint remain separate fields; no `provider:model` parsing is
  introduced.

## Decision table and test cases

| Cause/effect | T1 | T2 | T3 | T4 | T5 | T6 | T7 |
| --- | --- | --- | --- | --- | --- | --- | --- |
| C1 exact target | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C2 unique legacy | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C3 ambiguous legacy | 0 | 0 | 1 | 0 | 0 | 0 | 1 |
| C4 no match | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C5 both shapes | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C6 neither shape | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C7 one disabled | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| E1 exact triple | 1 | 1 | 0 | 0 | 0 | 0 | 0 |
| E2 legacy canonical | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| E3 ambiguous | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| E4 unresolved | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| E5 invalid request | 0 | 0 | 0 | 0 | 1 | 1 | 0 |
| E6 sole enabled | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

Automated evidence:

- T1-T4: config-resolver unit tests.
- T5-T6: admin-config request-shape tests.
- T7: profile endpoint-toggle integration test.
- UI/wire alignment: generated-contract freshness plus Web typecheck/build.
