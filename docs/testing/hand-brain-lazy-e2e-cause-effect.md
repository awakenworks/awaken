# Hand/Brain lazy Sandbox TS E2E cause-effect model

## Cause graph

```text
C1 native Environment binds on_tool_use policy
C2 Session is created
C3 resolved tool target = Brain (dynamic MCP)
C4 resolved tool target = Sandbox (built-in Hand)
C5 Sandbox create + binding persistence succeeds
C6 run is claimed through durable ingress
C7 current dispatch lease accepts the Sandbox binding
C8 cold Worker receives the frozen Session runtime projection
C9 Worker manifest honestly supports the frozen resource envelope

C1 & C2 -----------------------> E1 durable environment_binding remains null
E1 & C3 -----------------------> E2 MCP executes; binding remains null
E1 & C4 -----------------------> E3 caller waits for Sandbox materialization
E3 & C5 & C6 & C7 -------------> E4 dispatch binding commits first
E4 ----------------------------> E5 Session binding commits before Hand result
C6 & C8 & C9 & C3 -------------> E6 remote Brain run is claimed without Sandbox
C6 & C8 & C9 & C4 & C7 --------> E7 remote Hand run binds Sandbox before use
```

## Decision table

| Rule | Worker | Sandbox tier | Tool | Durable binding before | Durable binding after | Evidence |
|---|---|---|---|---|---|---|
| H1 | local durable | workdir | none | null | null | Environment policy API + SQLite aggregate |
| H2 | local durable | workdir | none after Session create | null | null | Session API + SQLite aggregate |
| H3 | local durable | workdir | dynamic MCP | null | null | `agent.mcp_tool_use` + fixture call + SQLite aggregate |
| H4 | local durable | workdir | read (before call) | null | null | SQLite aggregate before tool dispatch |
| H5 | local durable | workdir | read (after call) | null | provider handle | successful `agent.tool_result` + SQLite aggregate |
| W1 | remote database-less | workdir | dynamic MCP | false | false | TS events + Control `sandbox_bound` + absent Worker directory |
| W2 | remote database-less | workdir | read | false | true | TS events + Control `sandbox_bound` + Worker directory |
| W3 | remote database-less | namespace | dynamic MCP | false | false | TS events + Control `sandbox_bound` + absent Worker directory |
| W4 | remote database-less | namespace | read | false | true | TS events + Control `sandbox_bound` + namespaced Worker directory |
| W5 | remote database-less | Docker/Podman | dynamic MCP | false | false | TS events + Control `sandbox_bound` + absent named container |
| W6 | remote database-less | Docker/Podman | read | false | true | TS events + Control `sandbox_bound` + Worker-owned named container |

## Durable publication decision table

| Rule | Current claim | Dispatch bind | Session bind | Runtime publish | Physical Sandbox |
|---|---|---|---|---|---|
| D1 Brain-only | yes | not attempted | absent | no Sandbox | absent |
| D2 first Hand | yes | applied | succeeds | published | retained |
| D3 replaced claim | no | fenced | not attempted | rejected | disposed |
| D4 crash gap | yes | applied | fails | rejected | retained for adoption |
| D5 adoption retry | new owner | already bound/adopted | repaired | published | reused |

`e2e/hand_brain_lazy_e2e.ts` owns H1-H5 and exercises D1-D2 through the public
durable API. Rust decision tests own D3-D5 because those failure windows require
precise lease/sink fault injection. `e2e/hand_brain_remote_worker_e2e.ts` owns
W1-W6 with a real coordinator-only Control process and a real database-less
Worker. Its Worker installs a real resource plane; merely advertising
`session-resources/v1` would invalidate placement evidence. The dispatch API
exposes only `sandbox_bound`, never the opaque provider handle.

W5-W6 self-skip only when the selected external container runtime is unavailable;
`AWAKEN_E2E_REQUIRE_CONTAINER=1` turns that condition into a fail-closed CI error.

## Cold Worker reconstruction decision table

| Rule | Runtime envelope | Provisioning | Expected before tool | Result |
|---|---|---|---|---|
| C1 | absent legacy row | unknown | eager Sandbox | backward-compatible eager realization |
| C2 | valid | `on_tool_use` | no Sandbox | deferred executor + exact toolsets restored |
| C3 | malformed | unreadable | no Sandbox | fail closed before context construction |
