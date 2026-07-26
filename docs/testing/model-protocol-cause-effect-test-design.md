# Model protocol cause-effect test design

## Scope

This slice covers the model-catalog dialect boundary from its Rust enum through
adapter-family selection, JSON wire serialization, generated management
contracts, and the developer-facing authoring form. It deliberately does not
claim request/response protocol conformance; each concrete provider adapter
must prove that separately before an Offering is executable.

## Causes and effects

| ID | Cause |
| --- | --- |
| C1 | Dialect is `AnthropicMessages` |
| C2 | Dialect is `OpenAiChat` |
| C3 | Dialect is `OpenAiResponses` |
| C4 | Dialect is `Gemini` |
| C5 | Dialect is `VertexGemini` |
| C6 | JSON token is not a declared dialect |
| C7 | User authors an endpoint through the Models form |
| C8 | Contract generation runs under Windows Git Bash with non-ASCII schema text |

| ID | Effect |
| --- | --- |
| E1 | Adapter family is `anthropic` |
| E2 | Adapter family is `openai` |
| E3 | Adapter family is `gemini` |
| E4 | Adapter family is `vertex` |
| E5 | Wire token round-trips without aliasing |
| E6 | Unknown token is rejected fail-closed |
| E7 | The form and generated contract expose the same declared choices |
| E8 | The generator selects executable Python and decodes schema JSON as UTF-8 |

## Cause-effect graph and constraints

```text
C1 -> E1 & E5
(C2 | C3) -> E2 & E5
C4 -> E3 & E5
C5 -> E4 & E5
C6 -> E6
C7 -> E7
C8 -> E8
```

- **O**: exactly one of C1-C6 is true for one decoded dialect input.
- **M**: C6 masks adapter selection; an unknown token never defaults to Chat.
- **R**: E7 requires regenerated JSON Schema, OpenAPI and TypeScript artifacts.
- **R**: on Windows, C8 requires `python.exe`; the non-executable WindowsApps
  `python3.exe` shim must not mask it, and locale-default GBK must not decode
  the UTF-8 schema output.
- Chat and Responses share an adapter family but remain distinct wire values;
  neither may alias the other during serialization.

## Decision table and test cases

| Cause/effect | T1 | T2 | T3 | T4 | T5 | T6 | T7 | T8 |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| C1 Anthropic | 1 | 0 | 0 | 0 | 0 | 0 | - | - |
| C2 OpenAI Chat | 0 | 1 | 0 | 0 | 0 | 0 | - | - |
| C3 OpenAI Responses | 0 | 0 | 1 | 0 | 0 | 0 | - | - |
| C4 Gemini | 0 | 0 | 0 | 1 | 0 | 0 | - | - |
| C5 Vertex Gemini | 0 | 0 | 0 | 0 | 1 | 0 | - | - |
| C6 unknown token | 0 | 0 | 0 | 0 | 0 | 1 | - | - |
| C7 authoring form | - | - | - | - | - | - | 1 | - |
| C8 Windows generation | - | - | - | - | - | - | - | 1 |
| E1 anthropic family | 1 | 0 | 0 | 0 | 0 | 0 | - | - |
| E2 openai family | 0 | 1 | 1 | 0 | 0 | 0 | - | - |
| E3 gemini family | 0 | 0 | 0 | 1 | 0 | 0 | - | - |
| E4 vertex family | 0 | 0 | 0 | 0 | 1 | 0 | - | - |
| E5 exact round-trip | 1 | 1 | 1 | 1 | 1 | 0 | - | - |
| E6 rejected | 0 | 0 | 0 | 0 | 0 | 1 | - | - |
| E7 choices aligned | - | - | - | - | - | - | 1 | - |
| E8 executable Python | - | - | - | - | - | - | - | 1 |

Automated evidence:

- T1-T6: `awaken-model-catalog` unit tests.
- T7: generated-contract freshness check plus web typecheck/build.
- T8: execute the generator through Git Bash on Windows.
