# MCP Prompt Skill 因果图与判定表

## 范围与不变量

MCP Prompt 是远端提供、激活时才取回正文的 instruction-only Skill。它与文件型
`SKILL.md` 共用 `list_skills`/`Skill` 入口，但不伪造目录、参考文件或脚本能力。
其 catalog `environment=instruction_only`，不要求 Hand；只有声明
`environment: filesystem` 或 bundle 含 `SKILL.md` 之外文件的 Skill 才会物化到
Hand 可见目录。Session prepare 不创建 Sandbox，首次执行通过 Session lifecycle
锁阻塞等待唯一 Sandbox 就绪。
`prompts_as_skills` 是显式 opt-in，省略等同 `false`。开启后若后端或 MCP Server
不能兑现 Prompt 能力，必须 fail closed，不能静默退化为 tools-only。

## 因与果

| ID | 因 |
|---|---|
| C1 | `prompts_as_skills=true` |
| C2 | 运行时是 Native |
| C3 | MCP Server 的 `prompts/list` 可用 |
| C4 | Prompt 声明必填参数 |
| C5 | 激活提供完整、合法的命名参数 |
| C6 | `prompts/get` 成功 |
| C7 | 同名、同 URL Attachment 的开关值发生变化 |

| ID | 果 |
|---|---|
| E1 | 不调用 `prompts/list`，只保留普通 MCP tool 行为 |
| E2 | Prompt 元数据进入统一 Skill catalog，provenance 为 `mcp` |
| E3 | catalog 阶段不调用 `prompts/get`；激活时才调用 |
| E4 | `Skill` 返回远端 Prompt 指令正文和参数语义 |
| E5 | stage/activation 明确报错且不产生可用投影 |
| E6 | 开关变化进入 durable fingerprint，创建新 generation |
| E7 | 省略/关闭开关维持旧 wire shape，显式开启可序列化往返 |

## 因果图

```text
¬C1 ------------------------------> E1 + E7
C1 ∧ C2 ∧ C3 --------------------> E2 + E3
C1 ∧ ¬C2 ------------------------> E5 (ACP fail closed)
C1 ∧ C2 ∧ ¬C3 ------------------> E5 (unsupported server)
E2 ∧ ¬C4 ∧ C6 ------------------> E4
E2 ∧ C4 ∧ C5 ∧ C6 --------------> E4
E2 ∧ C4 ∧ ¬C5 ------------------> E5 (no prompts/get)
E2 ∧ ¬C6 ------------------------> E5
C7 --------------------------------> E6
```

约束：C2 是 Native/ACP 的互斥选择；C5 仅在 C4 为真时有意义；C6 仅在已经
建立 E2 后有意义。发现 Prompt 元数据不等于读取指令正文，E2 必须先于 E3/E4。

## 判定表

`-` 表示该因被更早的条件遮蔽。

| 因/果 | T1 | T2 | T3 | T4 | T5 | T6 | T7 | T8 |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| C1 开启 | 0 | 1 | 1 | 1 | 1 | 1 | - | - |
| C2 Native | - | 1 | 1 | 0 | 1 | 1 | - | - |
| C3 list 可用 | - | 1 | 0 | - | 1 | 1 | - | - |
| C4 有必填参数 | - | 1 | - | - | 1 | 1 | - | - |
| C5 参数合法 | - | 1 | - | - | 0 | 1 | - | - |
| C6 get 成功 | - | 1 | - | - | - | 0 | - | - |
| C7 开关变化 | - | - | - | - | - | - | 1 | 0 |
| **E1 tools-only** | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E2 catalog 可见** | 0 | 1 | 0 | 0 | 1 | 1 | 0 | 0 |
| **E3 lazy get** | 0 | 1 | 0 | 0 | 1 | 1 | 0 | 0 |
| **E4 指令成功** | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E5 fail closed** | 0 | 0 | 1 | 1 | 1 | 1 | 0 | 0 |
| **E6 新 generation** | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E7 wire 兼容** | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 1 |

## 可执行测试映射

| 用例 | 测试锚点 |
|---|---|
| T1 | `prompt_skill_projection_tests::disabled_flag_never_discovers_prompts`；wire 默认值测试 |
| T2 | `enabled_flag_discovers_and_lazily_activates_one_unified_skill`；MCP registry 参数测试 |
| T3 | `enabled_flag_fails_closed_when_server_has_no_prompt_capability` |
| T4 | runtime-host H19：ACP 返回 `mcp_prompt_skills_unsupported` 且无投影 |
| T5 | MCP registry 的缺失/未知/非标量参数测试，并断言没有 `prompts/get` |
| T6 | MCP registry 的远端 get 错误传播测试 |
| T7 | session-contract D8：开关切换创建 generation 2 |
| T8 | `mcp_prompt_skill_switch_is_opt_in_and_wire_compatible` |

UI 验证另需覆盖两条入口：Agent 集成编辑器和新建 Session 内联 MCP；两处均默认
关闭、可见可切换，并把布尔值送入同一个配置/Session API 字段。
