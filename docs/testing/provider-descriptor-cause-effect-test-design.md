# Provider descriptor cause-effect test design

## Scope

Provider descriptors are static, secret-free capabilities used to render model
source cards and forms. Reading them must never author catalog or vault state.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | Client reads the descriptor endpoint |
| C2 | Descriptor has multiple supported protocols |
| C3 | Descriptor publishes a default URL for a dialect |
| C4 | Descriptor publishes form fields |
| C5 | Descriptor endpoint is read without prior configuration |

| ID | Effect |
| --- | --- |
| E1 | A secret-free descriptor list is returned |
| E2 | Protocol order is stable and OpenAI prefers Responses while retaining Chat |
| E3 | Every default URL uses one declared dialect and carries no parallel endpoint-id suffix |
| E4 | Provider kinds and field keys are unique |
| E5 | Provider, endpoint, offering, and credential stores remain unchanged |

## Graph, constraints, and decision table

```text
C1 -> E1
C2 -> E2
C3 -> E3
C4 -> E4
C5 -> E5
```

| Rule | T1 | T2 | T3 | T4 | T5 |
| --- | --- | --- | --- | --- | --- |
| C1 read | 1 | 0 | 0 | 0 | 0 |
| C2 multiple protocols | 0 | 1 | 0 | 0 | 0 |
| C3 default endpoint | 0 | 0 | 1 | 0 | 0 |
| C4 fields | 0 | 0 | 0 | 1 | 0 |
| C5 empty configuration | 0 | 0 | 0 | 0 | 1 |
| E1 list | 1 | - | - | - | - |
| E2 stable protocol preference | 0 | 1 | 0 | 0 | 0 |
| E3 valid endpoint dialect | 0 | 0 | 1 | 0 | 0 |
| E4 unique keys | 0 | 0 | 0 | 1 | 0 |
| E5 no writes | 0 | 0 | 0 | 0 | 1 |

The model-catalog unit tests cover T2-T4. The admin-router HTTP test combines
T1 and T5 and then reads the catalog to prove that capability discovery did not
create executable configuration.
