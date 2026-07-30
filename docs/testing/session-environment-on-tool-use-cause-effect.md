# Session Environment `on_tool_use` cause-effect model

## Scope

This model covers Native Awaken Session context construction and the first tool
invocation. It does not make ACP, delegation, Memory, repositories, filesystem
Skills, or other resource-bearing Sessions lazy: those capabilities require an
Environment while their runtime context is assembled and therefore stay eager.

## Cause graph

```text
C1 Environment.sandbox_provisioning = on_tool_use
C2 Session context has an eager Environment dependency
   (ACP | delegate | Memory | mount | repository | baseline mount/env |
    filesystem/fork Skill)
C3 current tool target = Brain
C4 current tool target = Sandbox
C5 an Environment is already published
C6 create/realize/persist succeeds
C7 concurrent Sandbox calls arrive

C1 & !C2 ------------------------> E1 build context without Environment
E1 & (text-only | C3) -----------> E2 do not create Environment
E1 & C4 & !C5 -------------------> E3 enter Session lifecycle mutex
E3 & C7 -------------------------> E4 exactly one creator; other calls wait/reuse
E3 & C6 -------------------------> E5 realize resources, persist binding, publish,
                                    then execute the tool
E3 & !C6 ------------------------> E6 dispose candidate, publish nothing, fail closed
!C1 | C2 ------------------------> E7 eager context construction
```

The durable binding is ordered before publication. Consequently neither a
concurrent tool call nor context lookup can observe a newly created Environment
whose binding was not committed.

## Decision table

| Rule | C1 | C2 | Invocation | Result | Test evidence |
|---|---:|---:|---|---|---|
| L1 | yes | no | text only | context and turn finish with no Environment | `on_tool_use_text_only_turn_keeps_the_environment_absent` |
| L2 | yes | no | Brain Skill tool | execute in Brain; no Environment | `on_tool_use_brain_skill_call_keeps_the_environment_absent` |
| L3 | yes | no | first Sandbox tool | block, create, publish, then invoke | `on_tool_use_runtime_hand_call_materializes_before_tool_execution` |
| L4 | yes | no | concurrent first Sandbox tools | one creation/binding; callers reuse | `on_tool_use_concurrent_hand_calls_create_and_persist_one_environment` |
| L5 | yes | no | Sandbox tool, binding fails | dispose and fail; publish nothing | `on_tool_use_binding_failure_never_publishes_the_environment` |
| L6 | no | any | first context/turn | eager Environment | `prepare_session_is_lazy_and_first_turn_materializes_the_environment` |
| L7 | yes | yes | context construction | conservative eager Environment | `on_tool_use_filesystem_skill_forces_an_eager_environment` |

## Invariants

1. Tool placement is decided by the registered `RawTool::execution_target`.
   Missing/unknown targets default to Brain and cannot awaken a Sandbox.
2. The deferred executor is installed only for `on_tool_use`; eligibility is
   recalculated after all frozen Session inputs are staged.
3. A filesystem or fork Skill never receives a fabricated `${SKILL_DIR}` or an
   absent auxiliary-run Environment.
4. Terminal cleanup owns the Environment from the Session slot even when the
   already-built context predates materialization and therefore stores `env=None`.
5. Environment loss evicts both eager and deferred runtime contexts; the next
   valid call rebuilds through the same lifecycle mutex.
