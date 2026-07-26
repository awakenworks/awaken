# Model attribute provenance cause-effect test design

## Scope

Model properties are optional published facts, not connection prerequisites.
Unknown stays absent; Awaken does not guess values that a provider model-list API
does not return. Each known field carries trusted field-level provenance. This
slice covers context and output-token limits; commercial price belongs to a Cloud
offering/billing contract rather than intrinsic model metadata.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | Neither token attribute is supplied |
| C2 | A positive context window is supplied |
| C3 | A positive max-output limit is supplied |
| C4 | Both are supplied and output is not greater than context |
| C5 | Either supplied value is zero |
| C6 | Output is greater than context |
| C7 | HTTP client supplies a provenance field |
| C8 | A legacy stored value has no provenance |
| C9 | PUT omits a field that was previously present |
| C10 | Signed-in Cloud projection supplies a token attribute |
| C11 | A later Cloud projection omits or removes that model |
| C12 | A manual value already exists when Cloud refreshes |

| ID | Effect |
| --- | --- |
| E1 | Attribute remains unknown and has no provenance |
| E2 | Supplied fields are stored and stamped `manual` with server time |
| E3 | Write fails atomically as invalid metadata |
| E4 | Forged provenance is rejected at the JSON boundary |
| E5 | Legacy value remains readable with unknown provenance |
| E6 | PUT replacement clears the omitted value and its provenance |
| E7 | Cloud value is cached with `brokered` provenance and observation time |
| E8 | Only stale `brokered` values are cleared |
| E9 | Manual value remains authoritative |

## Cause-effect graph and constraints

```text
C1 -> E1
(C2 | C3 | C4) -> E2
(C5 | C6) -> E3
C7 -> E4
C8 -> E5
C9 -> E6
C10 -> E7
C11 -> E8
C12 -> E9
```

- **I**: C1, C2, C3, and C4 describe mutually exclusive valid input classes.
- **M**: C5/C6 mask persistence; an invalid write leaves no partial fact.
- **M**: HTTP authoring always stamps `manual`; clients cannot claim
  `provider_api` or `curated` authority.
- **R**: provenance may reference only a populated, declared field.
- Empty/absent means unknown, not zero, unlimited, or a provider default.

## Decision table and test cases

| Cause/effect | T1 | T2 | T3 | T4 | T5 | T6 | T7 | T8 | T9 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C1 none | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C2 context only | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C3 output only | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C4 both valid | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C5 zero | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C6 output > context | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C7 forged provenance | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C8 legacy record | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C9 replace omission | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| E1 unknown | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| E2 trusted manual stamp | 0 | 1 | 1 | 1 | 0 | 0 | 0 | 0 | 0 |
| E3 atomic invalid | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 0 | 0 |
| E4 reject forged source | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| E5 legacy readable | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| E6 omitted field cleared | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

Cloud projection extension:

| Cause/effect | B1 | B2 | B3 |
| --- | --- | --- | --- |
| C10 Cloud known value | 1 | 0 | 1 |
| C11 later absent | 0 | 1 | 0 |
| C12 manual exists | 0 | 0 | 1 |
| E7 brokered stamp | 1 | 0 | 0 |
| E8 clear only brokered | 0 | 1 | 0 |
| E9 preserve manual | 0 | 0 | 1 |

Automated evidence:

- T1-T4, T8: model-catalog unit tests.
- T5-T7, T9: admin-config HTTP tests and repository validation.
- Contract/UI alignment: generated-contract freshness and Web typecheck/build.
- B1-B3: brokered aggregate and admin refresh tests; Cloud owns the upstream
  route-publication projection test.
