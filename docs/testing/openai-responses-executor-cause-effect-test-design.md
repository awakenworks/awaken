# OpenAI Responses executor cause-effect test design

## Scope

The `open_ai_responses` dialect has a dedicated executor. It posts to
`/responses`, preserves the publication-pinned model, disables provider-side
storage, maps neutral messages/tools onto Responses input items, and folds the
Responses output vocabulary back into Awaken's neutral runtime contract.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | Request contains text-only messages |
| C2 | Request contains URL/base64 images |
| C3 | Transcript contains a function call and function result |
| C4 | Request publishes tool descriptors |
| C5 | Response contains output text |
| C6 | Response contains a function call with valid JSON arguments |
| C7 | Response is incomplete because of max output tokens |
| C8 | Response reports token usage including cached input |
| C9 | Provider returns malformed output/function arguments |
| C10 | Provider returns a non-success HTTP status |

| ID | Effect |
| --- | --- |
| E1 | POST targets `<base>/responses` with Bearer auth, exact model, `store=false` |
| E2 | Messages/images map to Responses input items |
| E3 | Function call/result history maps to protocol-native items |
| E4 | Tool schema maps to a Responses function tool |
| E5 | Text becomes assistant text blocks |
| E6 | Function call becomes a tool-use block and `ToolUse` stop reason |
| E7 | Stop reason is `MaxTokens` |
| E8 | Neutral prompt/completion/cache-read usage is populated |
| E9 | Invalid provider payload fails closed |
| E10 | HTTP failure is classified into the neutral runtime error vocabulary |

## Cause-effect graph and constraints

```text
C1 -> E1 & E2
C2 -> E2
C3 -> E3
C4 -> E4
C5 -> E5
C6 -> E6
C7 -> E7
C8 -> E8
C9 -> E9
C10 -> E10
```

- The adapter never calls Chat Completions and never lets the SDK reselect a model.
- Tool arguments cross the untyped boundary only as JSON and fail closed if the
  provider returns invalid JSON.
- `store=false` is mandatory for every request; persisted response state is an
  Awaken runtime concern, not provider-side implicit state.
- The initial implementation uses the neutral executor's faithful degenerate
  stream: committed response semantics are complete while true Responses SSE is
  a separately testable optimization.

## Decision table and test cases

| Cause/effect | T1 | T2 | T3 | T4 | T5 | T6 | T7 | T8 | T9 | T10 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C1 text | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C2 image | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C3 call/result history | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C4 tool schema | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C5 output text | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C6 output call | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C7 incomplete max | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C8 usage | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C9 malformed | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C10 HTTP error | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| E1 exact POST | 1 | - | - | - | - | - | - | - | 0 | 0 |
| E2 message mapping | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| E3 call history | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| E4 tool mapping | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| E5 text output | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| E6 tool output | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| E7 max tokens | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| E8 usage | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| E9 invalid payload | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| E10 classified HTTP | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

The executable tests consolidate compatible rules without weakening the table:
request mapping covers T1-T4, response folding and stop-reason tests cover
T5-T8, malformed payload tests cover T9, and a localhost 401 response covers
T10. The localhost success test also observes the real request path, Bearer
header, exact model, and `store=false` rather than only testing a JSON helper.
