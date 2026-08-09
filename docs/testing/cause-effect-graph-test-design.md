# 因果图测试设计 · Awaken 运行时(1.0.0-dev)

> 方法:**因果图法(Cause-Effect Graphing)**。从现有代码提取输入条件(**因 / Cause**)与可观察的输出、状态迁移、错误(**果 / Effect**),
> 建立因→果的逻辑网(恒等 / 非 / 与 / 或)与约束(互斥 E、包含 I、唯一 O、要求 R、遮蔽 M),再由图归约推导**判定表**,每一列即一个测试用例。
>
> 规模:本设计覆盖 15 个子系统模块,共 **129 个因、127 个果**,归约出 **约 128 个判定表列(测试用例)**。每个因/果均锚定真实符号与文件位置。

---

## 0. 记法约定

**图逻辑关系**

| 记号 | 含义 |
|---|---|
| `a → E` | 恒等:因 a 为真则果 E 发生 |
| `~a` | 非:a 为假 |
| `a ∧ b → E` | 与:a、b 同真 |
| `a ∨ b → E` | 或:a、b 任一真 |
| `n1`、`n2` … | 中间节点(intermediate node) |

**约束(节点间)**

| 记号 | 名称 | 含义 |
|---|---|---|
| **E** | Exclusive 互斥 | 至多一个为真(≤1) |
| **I** | Inclusive 包含 | 至少一个为真(≥1) |
| **O** | One and only one 唯一 | 恰好一个为真(=1) |
| **R** | Requires 要求 | a 真则 b 必真(a⇒b) |
| **M** | Mask 遮蔽 | 一个果为真时遮蔽另一个果 |

**判定表单元格**:`1`=因成立/果发生;`0`=因不成立/果不发生;`-`=无关(don't care)。列头 `T#` 为测试用例编号。

**全局铁律(贯穿所有模块的 O 约束)**:一次 run 只能经由**恰好一个** `EndCause` 终结(`run.rs`)。`End` 枚举使 `Awaiting`(等待)与 `Ended`(终结)在构造上互斥,`finish` 永不可能把 `RunState::Awaiting` 与终结原因同时提交(`engine/mod.rs:1912`)。凡涉及 run 终结的果,均隐含此 O 约束。

---

## 模块 M1 · Agent 主循环(turn 执行 / stop-reason / 续跑 / 取消)

`crates/runtime/awaken-runtime/src/engine/{mod,dispatch,inference}.rs`

### 因(C1–C13)

| ID | 因 | 代码锚点 |
|---|---|---|
| C1 | 入口或循环中检测到取消 `is_cancelled()` | mod.rs:105 / 798 / 932;in-flight `tokio::select!` mod.rs:866 |
| C2 | 插件能力边界解析失败 `resolve_plugin_env` = Err | mod.rs:111 |
| C3 | 无模型 provider(executor 与 `llm()` 皆缺) | mod.rs:716 |
| C4 | 模型返回 `StopReason::ToolUse` / 含 tool_use 块(`tool_calls()` 非空) | dispatch, mod.rs:944 |
| C5 | 模型返回纯文本轮(`calls.is_empty()`) | mod.rs:965 |
| C6 | `MaxTokens` 截断、纯文本、且续跑预算未耗尽 | mod.rs:644 |
| C7 | `MaxTokens` 截断但仍携带 tool_calls(**遮蔽 C6 恢复**) | mod.rs:647 |
| C8 | 连续推理失败达到容忍上限 | mod.rs:897 |
| C9 | 步数上限耗尽(`0..max_steps` 走完,`end` 仍 None) | mod.rs:779 / 1089 |
| C10 | provider 熔断器打开 `breaker.check` = Err | inference.rs:223 |
| C11 | 工具执行 Err/未知/`catch_unwind` panic | dispatch.rs:1830 |
| C12 | run-end guard 裁决 = Steer / Complete / End | mod.rs:1001 |
| C13 | 暂存状态批冲突 `validate_batch` = Err | mod.rs:790 / 347 |

### 果(E1–E12)

| ID | 果 | 代码锚点 |
|---|---|---|
| E1 | 提交 `RunState::Ended(Cancelled)` | mod.rs:106 |
| E2 | `EndCause::Error(Failure::CapabilityBound)` | mod.rs:114 |
| E3 | `EndCause::Error(Failure::Inference{code,message})`(失败流终结) | mod.rs:897 |
| E4 | `EndCause::NaturalEnd`(纯文本收尾 / guard Complete-End) | mod.rs:1041 |
| E5 | `EndCause::MaxSteps` | mod.rs:1089 |
| E6 | `EndCause::Error(Failure::StateConflict)` | mod.rs:792 |
| E7 | 追加 assistant 消息入 ledger | mod.rs:957 |
| E8 | 提交截断片段 + 续跑提示,再次推理 | mod.rs:648 |
| E9 | 熔断快速失败 `Error::Provider` | inference.rs:224 |
| E10 | 发出 `ToolResult` 消息 | dispatch.rs:138 |
| E11 | 工具出错→模型可见错误结果 `ToolOutput::error` + span ERROR | dispatch.rs:1830 |
| E12 | 受导向续跑 `Fact::Continuation{steered:true}` + 审计 + `forced_continuations+=1` | mod.rs:1011 |

### 因果图与约束

```
C1 → E1                         C2 → E2                     C3 → E9(无 provider→drive 报错路径)
C4 ∧ ~C7 → (E10 ∨ E11)          C5 ∧ (guard=Complete/End) → E4
C6 ∧ ~C7 → E8                   C7 → (E7 截断轮成立, 不恢复)   ; M: C7 遮蔽 C6 的 E8
C8 → E3                         C9 → E5                     C10 → E9
C11 → E11                       C12=Steer → E12             C13 → E6
```

- **O**(终结原因):`E1,E2,E3,E4,E5,E6` 恰一发生。
- **R**:C6 恢复要求 `truncation_retries < max_continuation_retries`;失效则截断轮维持(等价 C9 分支)。
- **R**:C4(工具分支)与 C5(纯文本/边界分支)在一步内**互斥**(`calls.is_empty()` 二分,mod.rs:965 vs 1054)→ **E**{C4,C5}。
- **M 遮蔽**:C7 遮蔽 C6(携带工具调用时跳过截断恢复);C10 遮蔽任何推理重试(熔断在尝试前返回);首个 Steer guard 短路后续 guard 的 Complete。

### 判定表 M1

| 因\用例 | T1 | T2 | T3 | T4 | T5 | T6 | T7 | T8 | T9 |
|---|---|---|---|---|---|---|---|---|---|
| C1 取消 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C2 插件边界 | - | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C3 无 provider | - | - | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C4 tool_use | - | - | - | 1 | 0 | 0 | 0 | 0 | 0 |
| C5 纯文本轮 | - | - | - | 0 | 1 | 0 | 1 | 0 | 0 |
| C6 截断+预算 | - | - | - | - | - | 1 | 0 | 0 | 0 |
| C7 截断携带工具 | - | - | - | - | - | 0 | 0 | 0 | 0 |
| C8 失败流终结 | - | - | - | - | - | - | 0 | 1 | 0 |
| C9 步数上限 | - | - | - | - | - | - | 0 | 0 | 0 |
| C11 工具 panic | - | - | - | 0 | - | - | 0 | 0 | 0 |
| C12 guard=Steer | - | - | - | - | 0 | - | 1 | 0 | 0 |
| C13 状态冲突 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **果** | | | | | | | | | |
| E1 Cancelled | 1 | - | - | - | - | - | - | - | - |
| E2 CapabilityBound | - | 1 | - | - | - | - | - | - | - |
| E9 Provider 快失败 | - | - | 1 | - | - | - | - | - | - |
| E10 ToolResult | - | - | - | 1 | - | - | - | - | - |
| E4 NaturalEnd | - | - | - | - | 1 | - | - | - | - |
| E8 截断续跑 | - | - | - | - | - | 1 | - | - | - |
| E12 Steer 续跑 | - | - | - | - | - | - | 1 | - | - |
| E3 Inference 终结 | - | - | - | - | - | - | - | 1 | - |
| E6 StateConflict | - | - | - | - | - | - | - | - | 1 |

> 补充用例:T10 = C4∧C11(工具 panic→E11 而非 E10);T11 = C7=1(截断携带工具,验证 E8 被遮蔽、E7 截断轮成立)。

---

## 模块 M2 · 上下文压缩(Compaction)

`crates/runtime/awaken-ext-compact/src/{fold,plugin}.rs`

### 因(C14–C19)

| ID | 因 | 锚点 |
|---|---|---|
| C14 | token 窗口触发:`est_tokens ≥ trigger_ratio*max_tokens`(`max_tokens=Some`) | fold.rs:17 / plugin.rs:162 |
| C15 | 消息条数阈值触发:`committed_len > threshold`(`max_tokens=None`) | fold.rs:10 |
| C16 | `keep_last` 覆盖整段会话(可折叠段为 0) | fold.rs:30 |
| C17 | 本 run 已评估过压缩(`ContextMessages` 含 `COMPACT_PLUGIN_ID`) | plugin.rs:225 |
| C18 | 无 Agent 工具 / 摘要空 / 摘要 Err | plugin.rs:159 / 197 |
| C19 | 折叠成功产出摘要 | plugin.rs:233 |

### 果(E13–E16)

| ID | 果 | 锚点 |
|---|---|---|
| E13 | 注入"仅请求可见"摘要入 `ContextMessages` + 暂存 `compaction_marker`;`compaction_count+1` | plugin.rs:233 / 67 |
| E14 | 不折叠:记空 `ContextMessages` 条目(at-most-once 门) | plugin.rs:240 |
| E15 | 重放(不重折,不暂存)——已评估 | plugin.rs:225 |
| E16 | 无触发/整段被 keep_last 覆盖→`None` 不折 | fold.rs:30 |

### 因果图与约束

```
(C14 ∨ C15) ∧ ~C16 ∧ ~C17 ∧ ~C18 → E13
C16 → E16       C17 → E15(遮蔽 C14/C15)      C18 → E14
```

- **E**{C14,C15}:token 模式与条数模式由 `max_tokens=Some?` 唯一选择(plugin.rs:162)。
- **M**:C17 遮蔽 C14/C15——首次评估后任何步都不再折叠(晚增长的会话不折,plugin.rs:225)。

### 判定表 M2

| 因\用例 | T12 | T13 | T14 | T15 | T16 |
|---|---|---|---|---|---|
| C14 token 触发 | 1 | 0 | 1 | 1 | 0 |
| C15 条数触发 | 0 | 1 | 0 | 0 | 0 |
| C16 keep_last 全覆盖 | 0 | 0 | 1 | 0 | 0 |
| C17 已评估 | 0 | 0 | 0 | 1 | 0 |
| C18 无 runner/空摘要 | 0 | 0 | 0 | - | 1 |
| **E13 注入摘要** | 1 | 1 | 0 | 0 | 0 |
| **E16 不折(覆盖)** | 0 | 0 | 1 | 0 | 0 |
| **E15 重放(遮蔽)** | 0 | 0 | 0 | 1 | 0 |
| **E14 记空条目** | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M3 · 状态机 & Goal 扩展

`crates/runtime/awaken-ext-state-machine/src/{engine,plugin}.rs`、`awaken-ext-goal/src/lib.rs`

### 因(C20–C27)

| ID | 因 | 锚点 |
|---|---|---|
| C20 | 转移匹配且 current ∈ `from` → Allow | engine.rs:87 |
| C21 | 匹配但非允许,最强违规动作 = Deny / Ask / Warn | engine.rs:99(`action_rank`) |
| C22 | strict 机且无任何匹配转移 → Deny | engine.rs:127 |
| C23 | emit 处于冷却期 `on_cooldown` | plugin.rs:337 |
| C24 | 运行结束时存在非终止实例 且 续跑预算剩余 | plugin.rs:430 |
| C25 | Goal:`max_iterations == 0` | goal lib.rs:355 |
| C26 | Goal:无 deliverable 产出 | goal lib.rs:366 |
| C27 | Goal grader 裁决 = Satisfied / Failed / NeedsRevision(× 预算) | goal lib.rs:282 |

### 果(E17–E23)

| ID | 果 | 锚点 |
|---|---|---|
| E17 | 门 `Allow`(Warn/无) | plugin.rs:171 |
| E18 | 门 `Block{reason}`(Deny) | plugin.rs:171 |
| E19 | 门 `Suspend{ticket="fsm-…"}`(Ask) | plugin.rs:171 |
| E20 | `Transition` 应用 + `FsmMetricEvent::Transitioned` | engine.rs:242 |
| E21 | run-end `Steer`(收尾协议) / `Complete` | plugin.rs:435 |
| E22 | Goal `Complete{detail}`(满足/失败/达上限/空/零预算) | goal lib.rs:318 |
| E23 | Goal `Steer{feedback}`(NeedsRevision 且预算内) | goal lib.rs:388 |

### 因果图与约束

```
C20 → E17 ∧ E20      C21=Deny → E18    C21=Ask → E19    C22 → E18
C24 → E21            C25 ∨ C26 → E22   C27=NeedsRevision∧预算内 → E23   C27=其余 → E22
```

- **E**{`ViolationAction` Deny/Ask/Warn} 每转移唯一。
- **M 优先级**:`Deny > Ask > Warn`(`action_rank`,机内 engine.rs:99、跨机 gate_decision:152);单独 Warn 永不产生 E18(被遮为 Allow,后以警告呈现)。
- **M**:Goal 退化输入先于 grader——C25 遮蔽 C26/C27(不调 judge);C26 遮蔽 C27;`Failed` 立即终结不烧预算;grader Err → fail-open Failed。

### 判定表 M3

| 因\用例 | T17 | T18 | T19 | T20 | T21 | T22 | T23 |
|---|---|---|---|---|---|---|---|
| C20 允许转移 | 1 | 0 | 0 | 0 | - | - | - |
| C21 违规=Deny | 0 | 1 | 0 | 0 | - | - | - |
| C21 违规=Ask | 0 | 0 | 1 | 0 | - | - | - |
| C22 strict 无匹配 | 0 | 0 | 0 | 1 | - | - | - |
| C25 max_iter=0 | - | - | - | - | 1 | 0 | 0 |
| C26 无 deliverable | - | - | - | - | 0 | 1 | 0 |
| C27 NeedsRevision(预算内) | - | - | - | - | 0 | 0 | 1 |
| **E17 Allow** | 1 | 0 | 0 | 0 | - | - | - |
| **E18 Block** | 0 | 1 | 0 | 1 | - | - | - |
| **E19 Suspend** | 0 | 0 | 1 | 0 | - | - | - |
| **E22 Goal Complete** | - | - | - | - | 1 | 1 | 0 |
| **E23 Goal Steer** | - | - | - | - | 0 | 0 | 1 |

---

## 模块 M4 · 权限门(Permission)

`crates/runtime/awaken-ext-permission/src/lib.rs`

### 因(C28–C35)

| ID | 因 | 锚点 |
|---|---|---|
| C28 | Mode = BypassPermissions(规则前早返回) | lib.rs:168 |
| C29 | 命中规则 behavior = Deny | lib.rs:177 |
| C30 | 命中 allow/ask 规则且更具体 `spec > best_spec` | lib.rs:180 |
| C31 | 无规则命中 且 Mode = Plan | lib.rs:189 |
| C32 | 无规则命中 且 Mode ∈ {Default, AcceptEdits} → `default_behavior` | lib.rs:191 |
| C33 | 配置省略 `default_behavior`(默认 Ask) | lib.rs:229 |
| C34 | 模式用不支持的正则算子 `=~`/`!=~` | lib.rs:68 |
| C35 | `parse_pattern` 失败(策略畸形) | lib.rs:49 / 262 |

### 果(E24–E30)

| ID | 果 | 锚点 |
|---|---|---|
| E24 | `PermissionDecision::Allow` | lib.rs:210 |
| E25 | `PermissionDecision::Deny{reason}`(fail-closed) | lib.rs:211 |
| E26 | `PermissionDecision::Ask{ticket="perm-{call_id}"}` | lib.rs:216 |
| E27 | 未命中@Plan → Deny(fail-closed 默认) | lib.rs:189 |
| E28 | 未命中@Default/AcceptEdits → default_behavior(默认 Ask) | lib.rs:191 |
| E29 | 解析报错点名算子:"regex operator '{op}' is not supported…" | lib.rs:69 |
| E30 | `parse_ruleset` Err:"rule pattern '{pattern}': {e}"(畸形策略绝不静默放行) | lib.rs:262 |

### 因果图与约束

```
C28 → E24(强制放行, 遮蔽全部规则)
C29 → E25       C30 → (E24 ∨ E26)      C31 → E27      C32 ∧ C33 → E28(Ask)
C34 → E29       C35 → E30
```

- **O**{Mode}:BypassPermissions / Plan / Default / AcceptEdits 每 ruleset 恰一。
- **M 优先级**:**Deny 绝对**——单个 Deny 命中即返回,无视具体度(lib.rs:178);**Bypass 遮蔽全部规则**;**Plan 遮蔽 default_behavior**(未命中强制 Deny)。

### 判定表 M4

| 因\用例 | T24 | T25 | T26 | T27 | T28 | T29 | T30 |
|---|---|---|---|---|---|---|---|
| C28 Bypass | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C29 命中 Deny | - | 1 | 0 | 0 | 0 | 0 | 0 |
| C30 命中 allow/ask | - | 0 | 1 | 0 | 0 | 0 | 0 |
| C31 未命中@Plan | - | 0 | 0 | 1 | 0 | 0 | 0 |
| C32 未命中@Default | - | 0 | 0 | 0 | 1 | 0 | 0 |
| C34 正则算子 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C35 畸形策略 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E24 Allow** | 1 | 0 | 1 | 0 | 0 | 0 | 0 |
| **E25 Deny** | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E27 Plan→Deny** | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E28 default(Ask)** | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E29 算子报错** | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E30 策略报错** | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M5 · 凭证栅栏 & 出口铸造(Credential / Egress)

`crates/runtime/awaken-credential`、`crates/control/awaken-credential-vault`

### 因(C36–C45)

| ID | 因 | 锚点 |
|---|---|---|
| C36 | source status ≠ Active | lib.rs:384 |
| C37 | kind=Vault 但 `material_ref`=None | lib.rs:389 |
| C38 | kind=Vault,ref 存在但 store 缺失 | `SecretStore::get` miss |
| C39 | kind=Env,`env_key` 设置但宿主变量未设 | lib.rs:400 |
| C40 | kind=Oauth 但无 `oauth_command` / feature 关 / helper 失败 | lib.rs:411 / 419;oauth.rs:85 |
| C41 | 可用性:cooled 且 `now < retry_at` | availability.rs:101 |
| C42 | 可用性:cooled 且 `now ≥ retry_at`(自动恢复) | availability.rs:101 |
| C43 | 池成员 `enabled=false` | lib.rs:192 |
| C44 | 封印 blob 长度 < NONCE_LEN / 错密钥 / 篡改 / 非 UTF-8 | sealed.rs:74/80/82 |
| C45 | 封印密钥:inline+file 同设 或 皆未设 | sealed.rs:120 / 128 |

### 果(E31–E38)

| ID | 果 | 锚点 |
|---|---|---|
| E31 | `Ok(RedactedString)` 在注入接缝物化 | lib.rs:380 |
| E32 | `NotActive`(禁用/归档 fail-closed) | lib.rs:384 |
| E33 | `MissingMaterialRef` / `SecretNotFound` / `MissingEnv` | lib.rs:389/… |
| E34 | `OAuth(...)`(无命令/feature关/helper失败/空 token/spawn) | lib.rs:411 |
| E35 | `AvailabilityState` = Available / CooledDown{retry_at} / Exhausted | availability.rs |
| E36 | cooled 源被移出 `eligible_order`;过期后自动移回 | lib.rs:206 |
| E37 | `Seal` 错误(错密钥/篡改/截断/非UTF8 fail-closed) | sealed.rs:80 |
| E38 | 组合根拒绝启动:seal-key "mutually exclusive" / "brick every restart" | sealed.rs:120/128 |

### 因果图与约束

```
C36 → E32     C37 ∨ C38 → E33     C39 → E33     C40 → E34
C41 → E35(CooledDown) ∧ E36      C42 → E35(Available)     C43 → (移出 selection_order)
C44 → E37     C45 → E38     ~(C36∨C37∨C38∨C39∨C40) → E31
```

- **O**{CredentialKind Vault/Env/Oauth} 每 source 唯一;**O**{AvailabilityState Available/CooledDown/Exhausted} 在给定 `now_ms` 唯一;**E**{seal-key inline, file}(两者其一)。
- **R**:Vault 封印要求 `SecretStore::put` 不失败,否则 `Storage` 遮蔽 E31(无半创建的悬挂 ref 行)。
- **M**:`Exhausted` 遮蔽任何期限直至显式 `clear`;更新鲜的 cool_down 遮蔽更早期限(availability.rs:77)。

### 判定表 M5

| 因\用例 | T31 | T32 | T33 | T34 | T35 | T36 | T37 | T38 |
|---|---|---|---|---|---|---|---|---|
| C36 非 Active | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C37 Vault 缺 ref | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C39 Env 变量缺 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C40 OAuth 失败 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C41 cooled 未到期 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C44 封印损坏 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C45 seal-key 冲突 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| 全部有效 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E32 NotActive** | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E33 Missing*** | 0 | 1 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E34 OAuth** | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| **E35/E36 CooledDown+移出** | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E37 Seal** | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E38 拒绝启动** | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E31 物化成功** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M6 · 租户作用域 & Authz 强制(Tenancy)

`crates/contract/awaken-tenancy`、`crates/control/awaken-authz-enforce`

### 因(C46–C54)

| ID | 因 | 锚点 |
|---|---|---|
| C46 | `Authority.reachable` 为空 | tenancy lib.rs:197 |
| C47 | 无选择器 且 authority 单例 | lib.rs:207 |
| C48 | 无选择器 且 authority 宽(>1) | lib.rs:209 |
| C49 | 任一选择器命中未被 `covers` 的作用域(fail-closed,逐个检查非首中即止) | lib.rs:214 |
| C50 | 所有选择器被覆盖但目标不一致 | lib.rs:224 |
| C51 | guard:无 bearer / x-api-key 头 | authz lib.rs:230 |
| C52 | guard:`authenticate` = Err(未知/过期/吊销 token) | lib.rs:233 |
| C53 | HTTP 方法 GET/HEAD→`agent.read`,否则→`agent.write` | lib.rs:198 |
| C54 | 策略 `AuthorizationDecision` = Allow / Deny / RequireApproval | lib.rs:161 |

### 果(E39–E46)

| ID | 果 | 锚点 |
|---|---|---|
| E39 | `Ok(ScopeId)` 单一授权目标 | lib.rs:222 |
| E40 | `NoAuthority`(fail-closed,principal 触达为空) | lib.rs:197 |
| E41 | `NotAuthorized{selected}`(窄 token 栅栏,不被授权 peer 遮蔽) | lib.rs:214 |
| E42 | `SelectionRequired`(歧义/须点名 workspace) | lib.rs:209/224 |
| E43 | HTTP 401 "missing bearer credential" | lib.rs:231 |
| E44 | HTTP 401 "invalid credential" | lib.rs:234 |
| E45 | HTTP 403(栅栏 resolve_scope Err **或** 策略 Deny) | lib.rs:250/265 |
| E46 | 放行 `next.run` + 写回 `RequestTenancy` 戳记 | lib.rs:260 |

### 因果图与约束

```
C46 → E40     C47 → E39     C48 → E42     C49 → E41     C50 → E42
C51 → E43     C52 → E44     (~C51 ∧ ~C52 ∧ 栅栏通过 ∧ C54=Allow) → E46     栅栏Err ∨ C54∈{Deny,RequireApproval} → E45
```

- **O**{ScopeClaim vehicle}:FromToken 仅权威、FromPath/FromDomain 仅选择,token 永不能是选择(lib.rs:91);**O**{HTTP 方法→action}:GET/HEAD 与其余二分,总函数(路径永不参与→无未映射路由 fail-open)。
- **M 顺序**:三道闸串行——**认证失败(401)遮蔽作用域与策略**(guard 在 scope 推导前返回);**栅栏(out-of-tenant 选择 403)遮蔽策略引擎**(`authorize` 前短路);**RequireApproval 在会话边界坍缩为 Deny**(三态 IAM 决策收窄为二态,fail-closed,lib.rs:167)。

### 判定表 M6

| 因\用例 | T39 | T40 | T41 | T42 | T43 | T44 | T45 |
|---|---|---|---|---|---|---|---|
| C51 无 bearer | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C52 认证失败 | - | 1 | 0 | 0 | 0 | 0 | 0 |
| C46 无权威 | - | - | 1 | 0 | 0 | 0 | 0 |
| C48 宽权威无选择 | - | - | 0 | 1 | 0 | 0 | 0 |
| C49 越租户选择 | - | - | 0 | 0 | 1 | 0 | 0 |
| C54=Deny | - | - | 0 | 0 | 0 | 1 | 0 |
| C47 单例授权 + Allow | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E43 401 missing** | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E44 401 invalid** | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E40 NoAuthority** | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| **E42 SelectionRequired** | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E41/E45 越租户 403** | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E45 策略 403** | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E46 放行+戳记** | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M7 · 数据主体 / GDPR(Data-Subject)

`crates/control/awaken-data-subject`

### 因(C55–C61)

| ID | 因 | 锚点 |
|---|---|---|
| C55 | consent:该用途有活跃 Granted 且无 Withdrawn | lib.rs:155 |
| C56 | consent:该用途存在 Withdrawn(否决,fail-closed,即便旁有陈旧 Granted) | lib.rs:150 |
| C57 | consent:仅 Pending / 无授予 / 未知主体(repo miss) | lib.rs:152 / 281 |
| C58 | erase:某 `ContentEraser::erase_subject` 返回 Err(戳记前 fail-closed) | lib.rs:295 |
| C59 | erase:主体在 repo 且 `put` 失败 | lib.rs:304 |
| C60 | erase:主体不在 repo(仍回执,内容扇出已完成) | lib.rs:302 |
| C61 | 捕获记录 `restricted=true`(Art.18) / TTL 到期非受限 | capture_store.rs:146 / 99 |

### 果(E47–E53)

| ID | 果 | 锚点 |
|---|---|---|
| E47 | `ContentCapture::Full`(仅活跃 Granted 无否决) | lib.rs:155 |
| E48 | `ContentCapture::Structured`(fail-closed 默认:撤回/待定/无授予/未知) | lib.rs:150 |
| E49 | `mark_erased`:每项 consent 翻 Withdrawn(留存审计)+ `erased_at` 戳记,记录保留非删除 | lib.rs:123 |
| E50 | `ErasureReceipt{records_removed}`(跨 eraser 求和) | lib.rs:308 |
| E51 | 失败 eraser 在会计戳记**前**抛错(未擦除绝不报告为已擦除) | lib.rs:295 |
| E52 | `ErasureError("accountability write failed…")`(repo put 失败) | lib.rs:304 |
| E53 | 受限行经 erase/sweep 均存活;TTL 到期非受限行被清 | capture_store.rs:146/99 |

### 因果图与约束

```
C55 → E47     C56 ∨ C57 → E48     C58 → E51     C59 → E52     C60 → E50(仍回执)
C61=restricted → E53(存活)
```

- **O**{ConsentStatus/用途}:写侧保持每用途 ≤1 grant(`upsert_consent` 取代),Granted/Pending/Withdrawn 互斥。
- **R/顺序**:C59 会计写要求 C58 内容扇出先成功(内容删除先于戳记)。
- **M**:**Withdrawn 遮蔽 Granted**(consent_ceiling 首个 Withdrawn 即 Structured);**失败 eraser 遮蔽成功回执与戳记**;**restricted 遮蔽擦除与 TTL sweep**。

### 判定表 M7

| 因\用例 | T46 | T47 | T48 | T49 | T50 | T51 |
|---|---|---|---|---|---|---|
| C55 活跃 Granted | 1 | 0 | 0 | - | - | - |
| C56 有 Withdrawn | 0 | 1 | 0 | - | - | - |
| C57 待定/未知 | 0 | 0 | 1 | - | - | - |
| C58 eraser 失败 | - | - | - | 1 | 0 | 0 |
| C59 repo put 失败 | - | - | - | 0 | 1 | 0 |
| C61 受限记录 | - | - | - | 0 | 0 | 1 |
| **E47 Full** | 1 | 0 | 0 | - | - | - |
| **E48 Structured** | 0 | 1 | 1 | - | - | - |
| **E51 戳记前抛错** | - | - | - | 1 | 0 | 0 |
| **E52 会计写失败** | - | - | - | 0 | 1 | 0 |
| **E53 受限存活** | - | - | - | 0 | 0 | 1 |

---

## 模块 M8 · 沙箱选择 / 隔离 / 挂载(Sandbox Provisioning)

`crates/worker/awaken-sandbox-{manager,container,local,memoryd}`、`awaken-provisioning-contract`

### 因(C62–C73)

| ID | 因 | 锚点 |
|---|---|---|
| C62 | 请求隔离级 > 后端能力(`caps.isolation < spec.isolation`) | prepare.rs:53 |
| C63 | 隔离下限未达 且 `on_unmet=FailClosed`(vs `DegradeWithConsent`) | sandbox.rs:286 |
| C64 | spec 要资源限额 但 tier 不能强制 | prepare.rs:89 |
| C65 | 保留 env key 出现 `RESERVED_ENV_KEYS` | prepare.rs:74 |
| C66 | `outputs_path` 非沙箱绝对路径 | prepare.rs:58 |
| C67 | k8s 设置 pids 限额(不可表达) | k8s.rs:143/498 |
| C68 | egress=Allowlist 但无 proxy | lib.rs:625 |
| C69 | spec 有 mounts(池不可用) | pool.rs:51 |
| C70 | 暖池对该 shape 有就绪容器 | pool.rs:230 |
| C71 | MemoryStore 挂载但无 mounter 接线 | provider.rs:250 |
| C72 | FUSE 可用(`/dev/fuse`+`fusermount`)vs copy 回退 | copy.rs:19 |
| C73 | 必需挂载解析为空(fail-closed)/ 内容哈希不符 | provider.rs:287 / lib.rs:708 |

### 果(E54–E63)

| ID | 果 | 锚点 |
|---|---|---|
| E54 | 准入 `Ok(EnvironmentPlan)` 携限额前行 | prepare.rs:93 |
| E55 | `InsufficientIsolation`(遮蔽低优先级故障) | prepare.rs:54 |
| E56 | `{ReadOnly/EgressSecret/NetworkIsolation/ResourceLimits/ReservedEnvKey/OutputsPathNotAbsolute}Unsupported` | prepare.rs:29 |
| E57 | `NoCapableBackend`(绝不降级) | sandbox.rs:188 |
| E58 | 同意降级:floor 下最强,`degraded_to=Some` | sandbox.rs:288 |
| E59 | k8s pids fail-closed `Backend("k8s cannot enforce … pids")`(遮蔽 ConfigMap+Pod 创建) | k8s.rs:499 |
| E60 | allowlist 无 no-bypass provider capability → `NetworkAllowlistUnsupported`；forward proxy 不能改变结果 | prepare.rs / lib.rs |
| E61 | 暖命中复用 `open_agent_from` / 未命中新建 `open_agent` / 非池化恒新建 | pool.rs:235 |
| E62 | MemoryStore 无 mounter → 高声失败 | provider.rs:250 |
| E63 | FUSE 挂载 vs copy 物化(可写者 teardown 收割);必需挂载不可解→fail closed | mounter.rs:108 / provider.rs:288 |

### 因果图与约束

```
C62 → E55     C64 → E56     C65 → E56     C66 → E56     (C62∧~c63 同意) → E58    C63 → E57
C67 → E59     (C68∧~C74) → E60     C69 → ~E61(暖) → 恒新建/非池化   C70 → E61(复用)   C71 → E62   C72 → E63   C73 → E63(fail closed)
```

- **O**{IsolationClass Workdir<Namespace<Container} 单值;**O**{NetworkPolicy Unrestricted/Allowlist/None};**O**{MountSource 七类之一};**E**{FUSE, copy}(`prefer_fuse=false` 强制 copy 无视 /dev/fuse)。
- **R**:C70 暖命中要求 C69 假(无 mounts,池化)且 `size>0`;C67 k8s pids fail-closed 要求 tier=k8s;C74 是任意进程无法绕过的网络 allowlist 证据，普通 forward-proxy env 不构成 C74；E62 要求 MemoryMounter 接线。
- **M 优先级**:`prepare_environment` 中隔离故障 E55 遮蔽保留键等低优先级(`an_isolation_violation_masks_a_lower_precedence_reserved_key`);k8s create pids fail-closed 遮蔽全部实现效果;admission `summary > required_field > reserved_key > writable_base`。

### 判定表 M8

| 因\用例 | T52 | T53 | T54 | T55 | T56 | T57 | T58 | T59 |
|---|---|---|---|---|---|---|---|---|
| C62 隔离不足(FailClosed) | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C62 隔离不足(同意降级) | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C65 保留 env key | 1 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C67 k8s pids | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C68 allowlist 且无 C74 no-bypass enforcement | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C70 暖池命中 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C71 无 mounter | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C73 必需挂载缺失 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E55 隔离不足** | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E58 同意降级** | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E56 保留键(被 E55 遮蔽)** | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E59 k8s pids 拒绝** | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| **E60 NetworkAllowlistUnsupported** | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E61 暖复用** | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E62 无 mounter 失败** | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E63 fail-closed** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

> T52 与 T54 对照展示遮蔽:C62∧C65 同真时仅 E55 触发,验证 M 优先级。

---

## 模块 M9 · 租约 / 回收 / 中毒 / 令牌(Lease · Reap · Poison · Token)

`awaken-sandbox-manager`、`awaken-connection-plan`、`awaken-tool-relay`

### 因(C74–C77)

| ID | 因 | 锚点 |
|---|---|---|
| C74 | 租约活性:Live / Expiring / Reapable | lease.rs:44 |
| C75 | 回收信号优先级:Revoked > Expired > TransportLost | lease.rs:125 |
| C76 | 协调三集:live∩ref(adopt)/ live∖ref(reap)/ ref∖live(orphan) | lease.rs:165 |
| C77 | 跨重启:容器 Exited > AgedOut(`age>max`)/ native-GC 空 | reaper.rs:42 |

### 果(E64–E67)

| ID | 果 | 锚点 |
|---|---|---|
| E64 | adopt(重连)已引用沙箱 / peer 再收养崩溃 worker 的活沙箱 | lease.rs:213 |
| E65 | reap 未引用沙箱(adopt→dispose);`ReapCause::{Revoked,Expired,TransportLost}` | lease.rs:219 / mgr lib.rs:128 |
| E66 | orphan 已引用但已死→重新放置 | lease.rs:196 |
| E67 | 跨重启 reaper:清 Exited/AgedOut、留年轻活体、native-GC no-op;失败 remove 重试 | reaper.rs:94/102 |

### 因果图与约束

```
C74=Reapable ∨ C75 → E65     C76(adopt) → E64     C76(orphan) → E66
C77 → E67
```

- **O**{ReapCause};**M 优先级**:`decide_reap` Revoked>Expired>TransportLost;`should_reap` Exited>AgedOut。
- **R**:C76 peer 再收养要求共享底座 tier(容器/k8s,沙箱寿命长于创建者);Workdir/local 恒 orphan(随属主死)。

### 判定表 M9

| 因\用例 | T60 | T61 | T62 |
|---|---|---|---|
| C74 Reapable | 1 | 0 | 0 |
| C76 adopt(live∩ref) | 0 | 1 | 0 |
| C76 orphan(ref∖live) | 0 | 0 | 1 |
| **E65 reap** | 1 | 0 | 0 |
| **E64 adopt** | 0 | 1 | 0 |
| **E66 orphan 重放置** | 0 | 0 | 1 |

---

## 模块 M10 · 派工队列 · claim / settle / fence(Dispatch)

`crates/server/awaken-run-ingress`(`worker.rs`、`memory.rs`、`postgres.rs`)

### 因(C82–C90)

| ID | 因 | 锚点 |
|---|---|---|
| C82 | 提交的等待票 = ScheduledAction(含崩溃恢复) | worker.rs:258 |
| C83 | 停在 input 票上 且待处理输入相关性匹配 | worker.rs:277/281 |
| C84 | 无票 且已提交 run 记录已 `Ended`(恢复的终态) | worker.rs:323 |
| C85 | 过期租约的 running 行(恢复,花一次崩溃重试) | memory.rs:157 |
| C86 | awaiting 行且到期待处理输入(唤醒)且线程未运行 | memory.rs:165 |
| C87 | 线程已有运行中 run(单写者/线程,遮蔽 wake/fresh) | memory.rs:148 |
| C88 | settle epoch ≠ 当前 lease_epoch → Fenced(否则 Applied) | memory.rs:385 |
| C89 | 提交栅栏:完整 `RunClaim(run, owner, epoch)` 不匹配 → `CommitError::Rejected` | commit_fence.rs:64 |
| C90 | reap:过期 且 `attempt_count ≥ max_attempts` → 死信 | memory.rs:420 |

### 果(E71–E78)

| ID | 果 | 锚点 |
|---|---|---|
| E71 | 进程内执行调度动作(fenced ctx) | worker.rs:175 |
| E72 | resume an awaiting Run from matching input | worker.rs:285 |
| E73 | settle Done(终态):移除派工 + 全部 pending | worker.rs:448 |
| E74 | settle Awaiting(检查点):留行,`attempt_count=0`,仅弃 consumed | memory.rs:399 |
| E75 | `Running` 结果→高声失败 `Error::Execution` | worker.rs:553 |
| E76 | `SettleOutcome::Fenced`→陈旧属主放弃(遮蔽 E73/E74) | worker.rs:453 |
| E77 | claim 返回 `Claimed{...}` 并递增 `lease_epoch`;恢复重claim `attempt_count+1` | memory.rs:277 |
| E78 | reap→死信(`DeadLetter`,`dead_lettered_at`);requeue→pending 满预算 | memory.rs:423 |

### 因果图与约束

```
C82 → E71     C83 → E72     C84 → E73(良性已完成)    C85 → E77(+attempt+1)    C86 ∧ ~C87 → claim wake
C88=Fenced → E76(遮蔽)     C89 栅栏拒 → 拒 per-step commit     C90 → E78
Running结果 → E75
```

- **O**{提交协调后端 fs/sqlite/pg 每部署};**O**{dispatch store memory/pg/transport};**O**{wake local/nats/pg-notify,pg-notify 要求 pg store}。
- **R**:C86 wake 要求 awaiting ∧ 到期输入 ∧ 线程未运行(三者);C85 恢复 claim 唯一豁免"未运行"守卫(重owning 同行);C90 reap 要求过期 ∧ attempt 满(Awaiting 重置 attempt→检查点 run 永不死信);C89 以同一后端事务锁定完整 `RunClaim` 并提交，不再暴露可产生 TOCTOU 的 epoch 观测接口。
- **M**:`Fenced` 遮蔽 E73–E75(reclaimer 状态不可侵);单写者/线程(C87)遮蔽 wake/fresh;enqueue 幂等/去重遮蔽新建行(至少一次投递→恰好一次效果);wake 丢失被 poll 兜底遮蔽(只延不丢)。

### 判定表 M10

| 因\用例 | T67 | T68 | T69 | T70 | T71 | T72 | T73 | T74 |
|---|---|---|---|---|---|---|---|---|
| C82 ScheduledAction 票 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C83 输入匹配恢复 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C84 恢复终态 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C85 过期租约恢复 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C86 唤醒(线程空闲) | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C87 线程已运行 | 0 | 0 | 0 | 0 | 1 | 1 | 0 | 0 |
| C88 epoch 不符 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C90 reap 满预算 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E71 执行调度动作** | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E72 恢复 awaiting** | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E73 良性已完成** | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E77 claim+attempt+1** | 0 | 0 | 0 | 1 | 1 | 0 | 0 | 0 |
| **claim None(单写遮蔽)** | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E76 Fenced 放弃** | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E78 死信** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M11 · 持久化提交 · checkpoint / recover(Commit Coordinator)

`crates/stores/awaken-store-{fs,sqlite,postgres}`、StreamCheckpoint

### 因(C91–C97)

| ID | 因 | 锚点 |
|---|---|---|
| C91 | `commit.validate()` 失败(空 id / 跨 run\|thread 票) | fs:97 / sqlite:203 / pg:234 |
| C92 | 终态即最终:先前已提交 state = Ended → 拒后续提交 | fs:116 / sqlite:227 / pg:262 |
| C93 | 提交带 waiting 且 state=Awaiting → 插等待票,否则删票 | sqlite:430 / pg:364 |
| C94 | open 时恢复:日志重放 / 尾行残断 | fs:54 |
| C95 | 后端选择 fs(append+fsync)/ sqlite(write_lock)/ pg(FOR UPDATE) | — |
| C96 | Stream 检查点 store 已接线 vs 未 | worker.rs:86 |
| C97 | recover 时检查点存在 `get`=Some vs None | fs:223 |

### 果(E79–E85)

| ID | 果 | 锚点 |
|---|---|---|
| E79 | 拒后终态提交 `Error::Rejected("run … already terminal")` | fs:126 |
| E80 | durable 提交 + 事件追加(messages/state/events/run_fact 原子一写) | fs:140 / sqlite:365 / pg:287 |
| E81 | 原子插/删 waiting 票连同检查点 | sqlite:434 |
| E82 | recover 时从日志重放重建读模型;sqlite/pg `hydrate` | fs:46 |
| E83 | 服务 `committed_state` 投影(durable state_command 行,resumed run 重放会计) | sqlite:134 / pg:217 |
| E84 | 检查点 durable put(temp+fsync+rename);完成时 delete | fs:228 |
| E85 | 检查点存在→从中断步 resume;不存在→整步重跑 | fs:223 |

### 因果图与约束

```
C91 → 拒(validate)     C92 → E79     C93 → E81     ~C91∧~C92 → E80 ∧ E83
C94 → E82     C96 ∧ C97 → E85(resume)     C96 ∧ ~C97 → 整步重跑     ~C96 → 整步重跑
```

- **O**{提交后端};**R**:步级 resume(C96/C97)要求 `with_stream_checkpoint` 接线 且 recover 时检查点存在,否则中断步重跑。
- **M**:提交栅栏拒(M10-E76)与终态拒(E79)均遮蔽 E80/E81(无 durable 写);跨进程 fs/sqlite 下 C92 终态即最终被遮蔽(文档化,fs:110)。

### 判定表 M11

| 因\用例 | T75 | T76 | T77 | T78 | T79 | T80 |
|---|---|---|---|---|---|---|
| C91 validate 失败 | 1 | 0 | 0 | 0 | 0 | 0 |
| C92 已终态 | 0 | 1 | 0 | 0 | 0 | 0 |
| C93 await 提交 | 0 | 0 | 1 | 0 | 0 | 0 |
| C94 尾行残断恢复 | 0 | 0 | 0 | 1 | 0 | 0 |
| C96∧C97 检查点在 | 0 | 0 | 0 | 0 | 1 | 0 |
| 正常提交 | 0 | 0 | 0 | 0 | 0 | 1 |
| **拒(validate)** | 1 | 0 | 0 | 0 | 0 | 0 |
| **E79 拒终态** | 0 | 1 | 0 | 0 | 0 | 0 |
| **E81 插/删票** | 0 | 0 | 1 | 0 | 0 | 0 |
| **E82 重放重建** | 0 | 0 | 0 | 1 | 0 | 0 |
| **E85 步级 resume** | 0 | 0 | 0 | 0 | 1 | 0 |
| **E80 durable 提交** | 0 | 0 | 1 | 0 | 0 | 1 |

---

## 模块 M12 · 协议前门(ACP / A2A / AI-SDK / AG-UI / MCP / Managed)

`crates/server/awaken-protocol-*`、`awaken-run-executor-*`、`awaken-webhook*`

### 因(C98–C107)

| ID | 因 | 锚点 |
|---|---|---|
| C98 | ACP 中途 interrupt / 取消令牌转发 | acp `Injection::Interrupt` / executor 转发 |
| C99 | ACP 流式 HARD-limit 横幅(assistant 文本)→ `HardLimit(RateLimited)` | acp `classify_from_acp_error` |
| C100 | ACP `StopReason` 变体 EndTurn/Cancelled/Refusal/MaxTokens | `termination_from_stop_reason` |
| C101 | A2A 返回 `TaskState` Completed/Failed/Canceled/(Working\|InputRequired\|AuthRequired) | a2a `end_cause_of` |
| C102 | AI-SDK thread id 共享(去空白输入)vs 新铸 `thread-{seq}` | ai-sdk `process_request` |
| C103 | AI-SDK 决策态 Output/Error/Denied/Approved;client-executed vs built-in | `extract_decisions`/`to_resume` |
| C104 | AG-UI 工具结果 `error.is_some()` | ag-ui `to_resume` |
| C105 | MCP 门裁决 Allow/Block/SetResult/(Suspend\|Schedule) | mcp `gate_verdict` |
| C106 | Managed 会话 id 服务端铸造(跳过 `owns_thread`);archived 写(409) | managed `create_session`/`Archived` |
| C107 | webhook 响应 2xx vs 408/425/429/5xx vs 永久 3xx/4xx；订阅/密钥/状态权威故障；持久连败 ≥ 阈值；畸形签名密钥；SSRF/私网 | webhook `dispatch`/`WebhookStore::record_delivery`/`validate_endpoint_url` |
| C108 | ACP 策略 `RequireConfirmation`;进程/worker 在等待期间被替换 | ACP `PermissionAwait` → durable `ResumeTicket` → one-shot resumed resolver |
| C109 | webhook authored update/create/delete 与 delivery 并发；密钥 put/delete/apply/complete 故障；pending/orphan/missing/foreign 库存；材料在删除快照后并发可见；畸形字段 | `WebhookStore` mutation intent + managed recovery/inventory |

### 果(E86–E95)

| ID | 果 | 锚点 |
|---|---|---|
| E86 | interrupt/cancel 中止 run(reap Term→Kill)→`Cancelled`;deadline→`TimedOut` | acp `Supervisor::reap` |
| E87 | HARD-limit 保留为 RateLimited → `Failure::Inference{code:"acp_failure"}` | acp `failure_cause` |
| E88 | StopReason→中性终结→EndCause(Refusal→`Stopped("agent refused")`) | acp `end_cause` |
| E89 | A2A 诚实终态:Completed→NaturalEnd;Failed→Error;Canceled→Cancelled;其余→**Indeterminate**(绝不判成功) | a2a-executor |
| E90 | 跨协议共享:一 wire 起的 turn 可在同线程另一 wire 恢复/观察(单 `RunApplication`) | run_application.rs |
| E91 | Managed 会话铸造 id `sesn_`(跳过已拥线程);create 先 provision 后 insert,fail closed;`VaultNotFound`→404 | managed sessions.rs |
| E92 | 单一终结权威 `RunState`:Failed→投影 `RunFailed{code,message}`(防丢错静默空收尾) | run_application.rs |
| E93 | webhook 签名投递(Standard-Webhooks 头,稳定 `webhook-id`)；2xx→delivered+持久清零；408/425/429/5xx/网络→有界重试并保留 outbox；永久 3xx/4xx→单次 rejected；持久连败达阈值→原子禁用 | webhook dispatch + config-plane source |
| E94 | webhook 投递期 SSRF 守卫:解析并钉全局可路由,拒 loopback/非https | `ReqwestSender::guarded` |
| E95 | `session.error`(`SessionError::classify`)于 `outcome.failure`;生命周期扇出 IDLED/TERMINATED/DELETED | managed events.rs |
| E96 | ACP permission 请求提交 `ToolPermission` await 并释放进程/租约;恢复仅对 ticket 固定 call id 应用一次裁决 | acp executor permission replacement e2e |
| E97 | webhook authored patch 保留 operational/ref；create/delete durable saga 可跨重启收敛；库存首快照只删无保护的 webhook key、次快照诊断 missing；畸形输入零副作用拒绝 | M30 T172–T180 + M33 T190 |

### 因果图与约束

```
C98 → E86     C99 → E87     C100 → E88     C101 → E89     C102(共享) → E90
C105=Block/Suspend → is_error(遮蔽执行)     C106 → E91
C107=2xx → delivered / 可重试故障 → retry+pending / 永久响应 → rejected / 权威故障 → outbox pending(E93)
C107=SSRF → E94(拒 POST)
C108 → E96
```

- **O/E**:协议前门按路径互斥,但**全收敛于单一 `RunApplication`**(跨协议共享宿主);后端路由 `Native ⊕ Acp ⊕ Remote`;`Codec::Newline ⊕ Codec::Acp`(Acp 需 `real-acp` feature)。`StepOutcome` 只保存权威 `RunState`,协议终态由其投影,使"waiting ∧ failed"/"pending ∧ finished"不可表示。
- **已知缺口(M 遮蔽/功能空洞)**:
  - **A2A 无带内拒绝**——`to_resume` 把任何入站文本读作 `Confirm{allow:true}`,拒绝仅经 `tasks/cancel` 可达。
  - **A2A 无流式/推送**——card `streaming=false,push_notifications=false`;`Working/InputRequired/AuthRequired` 遮为 `Indeterminate`(无法轮询/await)。
  - **AG-UI 错误通道仅单串**——拒绝/错误只经 `ToolMessage.error` 表达,无结构化故障通道。
  - **MCP `Suspend`/`Schedule` 遮蔽**——外部客户端无 await run,fail closed 为模型可见 `is_error`。

### 判定表 M12

| 因\用例 | T81 | T82 | T83 | T84 | T85 | T86 | T87 | T88 | T89 | T90 |
|---|---|---|---|---|---|---|---|---|---|---|
| C98 ACP interrupt | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C99 HARD-limit | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C100 Refusal | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C101 A2A Working | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C102 AI-SDK 共享线程 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C104 AG-UI error | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C105 MCP Suspend | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C106 Managed archived 写 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C107 webhook 状态/权威故障 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C107 webhook SSRF | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E86 中止→Cancelled** | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E87 RateLimited** | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E88 Refusal→Stopped** | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E89 Indeterminate** | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E90 跨协议恢复** | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **AG-UI 拒(仅 error 串)** | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| **MCP Suspend→is_error** | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E91 archived→409** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E93 分类/持久计数/pending** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E94 SSRF 拒 POST** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M13 · 资源存储 · MCP 注入 · 模型配置解析(Resources / Config)

`crates/resources/awaken-{file,memory,skill}-store`、`awaken-ext-{skills,mcp}`、`awaken-{model-catalog,config-resolver,config-store}`

### 因(C108–C112)

| ID | 因 | 锚点 |
|---|---|---|
| C108 | 文件 id 含 `../`/非法字符(`safe_id` 失败)/ 记忆·技能 id 净化为空 | file-store `get`;memory `sanitize_stem` |
| C109 | CAS 变更：`update` 的 `base_sha` 不符（新 sha 相同则幂等，否则冲突）；copy conditional delete 的 `(id,sha)` 匹配 / stale / 已缺失 | memory `update` / `delete_if_match` / copy harvest |
| C110 | MCP 两服务器净化后命名空间冲突 / 工具名净化为空组件 | mcp `add` / `resolve` |
| C111 | 模型无 offering / dialect 不符 / offering 端点在 `disabled_endpoints` | resolver `resolve_offering`/`resolve_inference_toggled` |
| C112 | CredentialBinding None/Exact/OneOfCredentialPool;作用域凭证 `provider_id`≠offering;跨租户源;池全冷却;写作用域≠行作用域 | resolver `resolve_credential`;config-store `ScopedConfigRegistry` |

### 果(E96–E110)

| ID | 果 | 锚点 |
|---|---|---|
| E96 | 拒畸形 id→解析为空 `Ok(None)`/`Ok(false)`(绝不逃出 base) | file-store `get`/`delete` |
| E97 | 内容寻址存储 + 去重(同哈希早返回);原子 temp+rename 发布 | file-store `put` |
| E98 | 路径穿越写被夹在根下 `etc-passwd.bin`(`sanitize_stem`) | memory/skill store |
| E99 | CAS 冲突返回活 head 且不覆写；幂等 update 保 version；stale conditional delete 保留 head、已缺失 delete 为 no-op；copy 只删除物化快照内路径 | memory repository + copy harvest |
| E100 | 校验先于变更:`TooLarge`/`InvalidPath`/`PathConflict`(size 检查先于 id 查找) | memory repository `validate_size` |
| E101 | 仅呈现两技能工具 `Skill`+`list_skills`(catalog-free 稳定哈希) | ext-skills `tool.rs` |
| E102 | 路径触发后在 catalog 浮现条件技能;`model_invocable=false` 遮蔽激活(披露≠授权) | ext-skills `is_surfaced` |
| E103 | 以命名空间 id 注入 MCP 工具 `mcp__<server>__<tool>`;version bump 时重解析 | ext-mcp `resolve` |
| E104 | 跳过不可映射工具保留其余(逐工具故障隔离);拒重复/冲突服务器 | ext-mcp `resolve`/`McpManager::add` |
| E105 | MCP 结果三态→中性 `Execution`/`is_error`/`ok`;敏感字段标 `x-sensitive` 并 `[redacted]` | ext-mcp `sensitive.rs` |
| E106 | 解析 model offering→`ResolvedInference`(三元组+adapter+base_url+物化 RedactedString) | resolver |
| E107 | 拒认证他 provider 的 env/不兼容凭证 `IncompatibleCredential`(作用域 provider_id) | resolver `can_consume` |
| E108 | 池成员冷却轮换 / 全冷却→`NoEligibleCredential{cooled}`;跨模型故障转移 | resolver `eligible_order` |
| E109 | 拒跨作用域配置写(no-op)/ 隐藏跨作用域读(scoped SQL 守卫) | config-store |
| E110 | 内容寻址编译 `fingerprint_of` sha256(排除 name/description);Agent 输入在 Session 创建时一次合并/解析，提示只从 `ResolvedSessionResources` 生成 | config-resolver + session-contract |

### 因果图与约束

```
C108 → E96 ∨ E98     C109=不符∧非幂等 → E99     C109=幂等/已缺失 → no-op     C109=delete匹配 → 仅删除所见head     C110 → E104(拒/隔离)
C111 → E106(成功) ∨ ModelUnresolved     C112=不兼容 → E107     C112=全冷却 → E108     C112=跨作用域写 → E109
```

- **O**{CredentialBinding None/Exact/Pool};**O**{ToolMatcher Exact/Glob/Regex};**O**{ApiDialect Anthropic/OpenAi/Gemini};InputResourceId(File/MemoryStore/Repository) 每 binding 单变体。
- **R**:条件技能浮现(E102)要求 `PathActivations` 接线 ∧ 匹配 glob;`IncompatibleCredential`(E107)要求 offering_provider 存在 ∧ 源 `provider_id` 有作用域(无作用域/env 源 `provider_id=None` 对任何 provider 通过 `can_consume`)。
- **M**:编译优先级 **UnknownTool 遮蔽 UnresolvedModel**(工具先解析);`mcp__` 前缀目标遮蔽"目标未选中"检查(MCP override 恒编译);净化为空遮蔽该工具投影但不遮服务器;命名空间冲突遮蔽第二服务器;`validate_size` 遮蔽 `NotFound`;delivered provenance 戳记遮蔽自声明 frontmatter;`model_invocable=false` 遮蔽激活;否定算子作用于缺失字段→NoMatch(文档化 fail-open 钉)。

### 判定表 M13

| 因\用例 | T91 | T92 | T93 | T94 | T95 | T96 | T97 | T98 | T99 | T100 |
|---|---|---|---|---|---|---|---|---|---|---|
| C108 畸形/穿越 id | 1 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C109 CAS 不符非幂等 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| C109 CAS 幂等 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| C110 命名空间冲突 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C111 offering 成功 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C111 端点禁用/无 offering | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C112 不兼容凭证 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C112 池全冷却 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C112 跨作用域写 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E96 解析为空(file)** | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E98 穿越被夹(mem/skill)** | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E99 CAS Conflict** | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E99 幂等保 version** | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 | 0 |
| **E104 拒冲突服务器** | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E106 ResolvedInference** | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| **ModelUnresolved** | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E107 IncompatibleCredential** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| **E108 NoEligibleCredential** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| **E109 跨作用域写 no-op** | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M14 · Session 资源激活与回收

`awaken-session-contract::SessionResourceState`、Managed Session
`activate_inputs`/`reconcile_resource_activations`、`awaken-session-store`

### 因(C113–C118)

| ID | 因 | 锚点 |
|---|---|---|
| C113 | 创建或动态替换产生新的 resolved manifest | `SessionResourceState::prepare` |
| C114 | Host 对待应用 manifest 物化成功 | `activate_inputs` / `prepare_session` |
| C115 | 物化失败；子分支旧 manifest 回滚成功 vs 回滚也失败 | `activate_inputs` |
| C116 | 进程重启时存在 Resource 或 MCP 非终态恢复工作 | `reconcilable_sessions` |
| C117 | Session 终止；子分支 sandbox/兼容 Repo 清理成功 vs 失败 | `release_terminal_resources` |
| C118 | Active Session 调用 `GET /v1/files?scope_id=...` | Files router |

### 果(E111–E116)

| ID | 果 | 锚点 |
|---|---|---|
| E111 | 外部 IO 前 durable Prepared/Releasing；成功后 Active/Released | session contract + session store |
| E112 | 同步失败且回滚成功：active manifest 不变，新 generation=Failed | Managed resources |
| E113 | 回滚也失败：pending 与相同 revision 保留，禁止假装成功 | Managed resources |
| E114 | 重启按 owner envelope 重试同一 pending revision；不重新解析 Agent/当前资源配置 | Managed sessions |
| E115 | 终态永不再激活；清理失败保持 Releasing，成功后 Released；仅 Session 生成的兼容 Repo 被 tombstone | Managed sessions |
| E116 | Files GET 只投影 Artifact，不 publish Repository、不持久化 authored Skill | Files router + Session release |

### 因果图

```text
C113 -> E111
C114=成功 -> Active
C115=失败∧回滚成功 -> E112
C115=失败∧回滚失败 -> E113
C116 -> E114
C117=清理成功 -> E115(Released)
C117=清理失败 -> E115(Releasing)
C118 -> E116(read-only)
```

授权前门只提供已经认证/授权的 owner scope；激活状态机不接收 principal、
role、API key、PDP decision 或 policy。Workspace owner envelope 与资源状态
分别持久化，跨 Workspace 可见性仍由边界 PEP 和 repository ownership fence
负责。

### 判定表 M14

| 因\用例 | T101 | T102 | T103 | T104 | T105 | T106 | T107 |
|---|---:|---:|---:|---:|---:|---:|---:|
| C113 新 manifest | 1 | 1 | 1 | 0 | 0 | 0 | 0 |
| C114 物化成功 | 1 | 0 | 0 | 1 | 0 | 0 | 0 |
| C115 回滚成功 | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| C115 回滚失败 | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| C116 重启有未结算状态 | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| C117 终止且清理成功 | 0 | 0 | 0 | 0 | 1 | 0 | 0 |
| C117 终止且清理失败 | 0 | 0 | 0 | 0 | 0 | 1 | 0 |
| C118 Files GET | 0 | 0 | 0 | 0 | 0 | 0 | 1 |
| **E111 durable 两阶段** | 1 | 1 | 1 | 1 | 1 | 1 | 0 |
| **E112 active 不变** | 0 | 1 | 0 | 0 | 0 | 0 | 0 |
| **E113 pending 保留** | 0 | 0 | 1 | 0 | 0 | 0 | 0 |
| **E114 同 revision 恢复** | 0 | 0 | 0 | 1 | 0 | 0 | 0 |
| **E115 Released/Releasing** | 0 | 0 | 0 | 0 | 1 | 1 | 0 |
| **E116 Files GET 无资源写副作用** | 0 | 0 | 0 | 0 | 0 | 0 | 1 |

---

## 模块 M15 · 端到端集成不变量与测试真实性

`crates/contract/awaken-agent-contract`、`crates/runtime/awaken-runtime`、
`crates/runtime/awaken-observability`、Managed/Config/Resource API 与 `e2e/`

### 因(C119–C129)与果(E117–E127)

| 因 | 触发条件 | 果 / 消解后的可观察结果 |
|---|---|---|
| C119 | Native Run 经历多个 await/resume，ledger 已含完整 assistant step | E117 从已提交的完整 assistant message ID 求最大 step+1；跨 resume 不冲突，截断片段和其他 Run 不参与 |
| C120 | Memory GET 使用默认 `view=basic` / 显式 `view=full` | E118 basic 稳定省略内容；full 返回可校验内容，测试不得把省略误判为空 Memory |
| C121 | 对普通上传 File 请求下载 / 对生成 Artifact 请求下载 | E119 上传 File `downloadable=false` 且下载 400；Artifact 才可下载；相同 bytes 只在私有 blob 层去重，逻辑 File ID 独立 |
| C122 | Workdir 请求只读 File mount / 可写 Memory 或 Repository | E120 File 因底层无只读隔离证据而在推理前拒绝；Memory/Repository 走同一资源实现并可执行 |
| C123 | Session 显式绑定 Memory/Skill；随后 Agent 又产出新 Skill | E121 只冻结显式 binding；新 Skill 进入 catalog/version store，但不隐式改写 immutable Agent publication |
| C124 | 两个 Workspace 使用同一 portable Agent ID | E122 `(scope_id,id)` 各自写读、revision、凭据引用独立；跨 path 403、跨 scope miss 404 |
| C125 | 已发布 Agent 的 Session 模型覆盖缺失/相同/不同 | E123 缺失则继承、相同则接受原 route、不同则在 Session 持久化前 400，禁止拼接旧 backend/credential |
| C126 | HTTP 请求匹配参数化路由 | E124 span 使用 Axum `MatchedPath` 作为低基数 `http.route`；无 matched path 才回退 raw URI |
| C127 | E2E transport 失败、缺 terminal，或只找到历史 reply | E125 测试硬失败且只接受发送后新增 ID；catch 不得伪造空状态或回退旧终态 |
| C128 | 请求 beta-gated Managed API 且缺少/携带 beta header | E126 缺 header 得 4xx；测试携带官方 beta 后才可断言业务投影，避免访问错误体时 `undefined` 崩溃 |
| C129 | 旧 global remote-Hand 场景与当前 Brain/Hand publication/Worker 路径并存 | E127 删除旧场景及依赖，只保留 lazy Brain/Hand 与 remote Worker 两条当前权威验证 |

### 因果图、约束与判定表 M15

```text
C119 -> E117                 C120=basic/full -> E118
C121=upload/artifact -> E119 C122=File/Memory|Repo -> E120
C123 -> E121                 C124 -> E122
C125=absent/equal/different -> E123
C126 -> E124                 C127 -> E125
C128 -> E126                 C129 -> E127
```

M15 的约束和 T108–T118 判定表由
[端到端集成判定表](./end-to-end-integration-decision-table.md) 作为唯一来源维护。

## 汇总统计

| 模块 | 因数 | 果数 | 判定表用例 |
|---|---|---|---|
| M1 主循环 | 13 (C1–C13) | 12 (E1–E12) | T1–T11 |
| M2 压缩 | 6 (C14–C19) | 4 (E13–E16) | T12–T16 |
| M3 状态机/Goal | 8 (C20–C27) | 7 (E17–E23) | T17–T23 |
| M4 权限 | 8 (C28–C35) | 7 (E24–E30) | T24–T30 |
| M5 凭证/出口 | 10 (C36–C45) | 8 (E31–E38) | T31–T38 |
| M6 租户/Authz | 9 (C46–C54) | 8 (E39–E46) | T39–T45 |
| M7 GDPR | 7 (C55–C61) | 7 (E47–E53) | T46–T51 |
| M8 沙箱 | 12 (C62–C73) | 10 (E54–E63) | T52–T59 |
| M9 租约/回收/中毒 | 8 (C74–C81) | 7 (E64–E70) | T60–T66 |
| M10 派工 | 9 (C82–C90) | 8 (E71–E78) | T67–T74 |
| M11 提交/检查点 | 7 (C91–C97) | 7 (E79–E85) | T75–T80 |
| M12 协议前门 | 10 (C98–C107) | 10 (E86–E95) | T81–T90 |
| M13 资源/配置 | 5 (C108–C112) | 15 (E96–E110) | T91–T100 |
| M14 Session 资源激活 | 6 (C113–C118) | 6 (E111–E116) | T101–T107 |
| M15 集成不变量/测试真实性 | 11 (C119–C129) | 11 (E117–E127) | T108–T118 |
| **合计** | **129 因** | **127 果** | **~128 用例** |

## FMECA：按核心流程组织的系统视图

本节不另造失效测试清单，而是把上面的唯一因果图和判定表投影为
FMECA。评分是变更审查时使用的相对优先级，不是现场故障率：严重度
`S`、发生度 `O`、检出难度 `D` 均为 1（低）到 5（高），`RPN=S×O×D`。
所有分数均为现有控制生效后的残余风险；安全能力不存在时“拒绝运行”
算可用性失效，不算安全绕过。

### 静态结构视图

| 核心流程 | 权威边界 | 下游依赖与契约 | 一致性/安全边界 |
|---|---|---|---|
| F1 配置到可执行快照 | Control 配置聚合与 `awaken-config-resolver` | Catalog/Vault → executable Agent/Environment publication | Workspace 作用域、版本/指纹、高水位 |
| F2 身份到授权 | service-auth + tenancy + authz PEP/PDP | 所有 HTTP/Worker 前门 | 默认拒绝、身份与 scope 一致 |
| F3 Session 创建到投影 | Dream/Managed application + Session repository | immutable baseline、Resource/MCP generation、Environment snapshot | root CAS、claim epoch、realization lease |
| F4 提交到派工 | run-ingress + work store | enqueue → claim → attempt binding → settle | 原子 claim、owner/epoch fence、幂等 receipt |
| F5 Worker 控制通道 | worker registry + transport security + worker contract | signed request/response、heartbeat、recovery | Server 模式强认证、incarnation/lease fence |
| F6 Environment 与 Sandbox | environment application/image build + provisioning/provider | exact revision → image → plan → sandbox | capability admission、no-downgrade、资源回收 |
| F7 Agent 执行 | runtime engine + extensions + provider | inference → tools → checkpoint/await/end | cancellation、permission、step/retry budget |
| F8 持久化与恢复 | coordinator + store contracts/backends | atomic commit、read model、outbox、replay | CAS、terminal fence、backend conformance |
| F9 协议与通知 | ACP/A2A/AI-SDK/AG-UI/MCP/Managed + webhook | domain outcome ↔ wire outcome | 单一错误映射、断线恢复、SSRF/签名 |
| F10 终止与清理 | durable projection + resource reclaimer + provider dispose | terminal fact → revoke/drain/release/tombstone | cleanup committed 后才外显终态、可重试 |

### 动态行为视图

```text
配置提交 --CAS/作用域--> 发布高水位 --刷新--> 可执行快照
   -> 身份/授权 -> Session root CAS -> Resource/MCP/Environment 两阶段投影
   -> Run enqueue -> Worker claim + attempt credential binding
   -> capability admission -> Sandbox/ACP/Native 执行
   -> 原子 checkpoint/settle -> 协议投影/webhook
   -> terminal cleanup committed -> 对外终态

任一边失败：不进入下一状态；同一 identity/epoch 重试幂等；owner/epoch
变化后旧执行者失去提交权；恢复只读取 durable authority，不从临时投影反推。
```

### 端到端 FMECA（EF24–EF60 续表见[集成判定表](./end-to-end-integration-decision-table.md)）

| ID | 流程失效模式与系统影响 | 现有消解/处理 | 判定表与测试证据 | S/O/D/RPN |
|---|---|---|---|---|
| EF1 | 配置写成功但投影落后，Session 使用旧模型/环境 | durable command high-water；落后时增量或全量重放；刷新失败拒绝 admission | M13 T96–T100；环境投影 outage/reconcile 单测；management/config E2E | 4/1/2/8 |
| EF2 | 身份缺失、scope 混淆或 PDP 绕过导致跨租户访问 | Server 启动强制签名凭据；PEP 串联 authenticate→scope→authorize；repository owner fence | M6 T39–T45；Worker transport R1–R4；authz/tenancy 协议 E2E | 5/1/1/5 |
| EF3 | Session 部分创建或 Resource/MCP 新旧 generation 混用 | 一次 root CAS；immutable baseline；Prepared/Active/Failed/Releasing 状态；stale CAS 清理 | M14 T101–T107；G42-P01–P05；Session store conformance | 5/1/2/10 |
| EF4 | 队列响应丢失、重复 claim 或旧 Worker 提交，造成重复执行/错误凭证 | enqueue/claim/settle 幂等键；owner+epoch fence；credential binding 与 claim 原子提交 | M10 T67–T74；dispatch testkit 三后端；credential worker E2E | 5/1/1/5 |
| EF5 | Worker 通道可伪造、断线后旧 incarnation 继续工作 | 签名 transport、trust store、heartbeat lease、incarnation replacement、stale commit 拒绝 | Worker transport 单测/E2E；worker lifecycle；G42-P03/P04 | 5/1/1/5 |
| EF6 | Provider 能力虚报或降级，导致隔离/网络/secret custody 低于请求 | `prepare_environment` 能力合取；fail-closed；无隐式 fallback；strict substrate gate | M8 T52–T59；sandbox capability suite；G43-P07 | 5/2/1/10 |
| EF7 | 推理/工具 panic、取消或预算耗尽后 Run 卡住或双终结 | catch-unwind、取消 select、有限 retry/step、唯一 `EndCause`、原子 checkpoint | M1 T1–T11；M2–M4；runtime 单元与 deterministic E2E | 4/1/1/4 |
| EF8 | 提交只写部分事实、分页乱序、终态复活或后端语义漂移 | Coordinator 原子提交；terminal fence；14 项共享契约跑 inmem/fs/sqlite/postgres | M11 T75–T80；`awaken-store-conformance` 4 后端；Postgres strict gate | 5/1/1/5 |
| EF9 | 协议映射丢失 stop/error/HITL，重连重复事件，webhook 泄漏或 SSRF | domain outcome 单向映射；cursor/replay；签名；URL policy；永久/暂时错误分类 | M12 T81–T90；protocol suites；webhook E2E | 4/2/2/16 |
| EF10 | 终止已外显但 secret/route/mount/image 未清理，或失败清理被当成功 | terminal visibility 等待 cleanup commit；lease revoke；Releasing 可重试；精确 generation dispose | Dream cleanup 单测；M9/M14；resource reclaimer 与 managed restart E2E | 5/1/2/10 |
| EF11 | ACP MCP 声称 Worker 持密，但 workload 能绕过 relay 直连上游 | 只有 substitution 与 provider no-bypass 同时为真才 admission；内置 provider 当前全部拒绝该声明 | M5；M8 T56；G43-P07；ACP projected container fail-closed | 5/2/2/20 |
| EF12 | 测试脚本遗漏场景、吞掉失败或把缺基础设施当成功 | `e2e/package.json` 唯一编排；stage graph 唯一 obligation；strict release flags；checker fault injection | test-orchestration R1–R8；check-all | 5/1/1/5 |
| EF13 | ACP 各 Run 重用消息 ID，托管投影把后续轮当重放去重；E2E 若回退历史回复会进一步假通过 | ACP fact ID 绑定 durable Run+序号；同 Run 重放稳定、跨 Run 唯一；E2E 只接受发送后新增 ID，fixture 每输入行产生一轮 | `acp_message_ids_are_replay_stable_and_cross_run_unique`；namespace Session environment 多轮 E2E 判定表 | 4/1/1/4 |
| EF14 | Native 多次 await/resume 复用固定 step base，后续 assistant fact ID 冲突并被投影去重 | message ID 的解析归 `awaken-agent-contract`；resume 从同 Run 已提交完整 assistant step 最大值+1 继续，忽略截断和其他 Run | M15 T108；`repeated_scheduled_resumes_keep_every_assistant_fact`；scheduled resume E2E | 4/1/1/4 |
| EF15 | 用 Memory `view=basic` 的省略字段判断持久内容，造成“空 Memory”假失败或 catch 后假绿 | basic/full 视图契约显式分区；内容测试统一请求 `view=full`；移除 transport catch→空列表 | M15 T109/T116；memory durability/extraction/full-chain/resource-plane PG E2E | 4/1/1/4 |
| EF16 | 把上传 File 当可下载 Artifact，或把 blob 去重误当逻辑 File ID 去重 | 上传 File 明确 `downloadable=false`；Artifact 下载独立；相同 bytes 保持两个 owner-visible 逻辑 ID | M15 T110；managed resources API E2E | 3/1/1/3 |
| EF17 | Workdir 无只读隔离能力却挂载只读 File，导致写保护虚证 | capability admission 在推理前 fail-closed；只对 substrate 真正支持的可写 Memory/Repository 执行；Artifact 只读投影另测 | M15 T111；resource mount + sandbox provisioning E2E | 5/1/1/5 |
| EF18 | Memory/Skill catalog 写入隐式修改已发布 Agent，导致 Session 配置随时间漂移 | 显式 binding + immutable skill selection；catalog/version store 与 Agent publication 分离；唯一 durable SkillStore | M15 T112；managed full-chain E2E；scenario-host composition tests | 4/1/1/4 |
| EF19 | 仍按全局 Agent ID 做 owner-protected no-op，导致合法租户无法复用 portable ID | `(scope_id,id)` 为唯一存储身份；跨 path 403、跨 scope 404；同 ID 数据/版本/凭据独立 | M15 T113；config-store SQLite/Postgres tests；cross-tenant config E2E | 5/1/1/5 |
| EF20 | Session 用任意 model 字符串覆盖已发布 Agent，拼接旧 backend/credential route | 缺失覆盖继承；相同 ID 接受；不同 ID 在 Session 持久化前拒绝，须先发布新 route | M15 T114；protocol-managed 单测；managed model inherit E2E | 5/1/1/5 |
| EF21 | telemetry 把含资源 ID 的 raw URI 记为 `http.route`，造成高基数/敏感标识扩散 | 优先使用 Axum `MatchedPath` 模板；仅无匹配路由回退 URI；动态路由回归测试 | M15 T115；`dynamic_routes_record_the_matched_template`；trace capture E2E | 4/1/1/4 |
| EF22 | E2E 缺 beta header、接受历史 reply、吞 transport 错误或伪造 terminal，导致脚本崩溃/假绿 | 每个 beta 路由带官方 header；先断言 transport status；只收新 ID/真实 terminal；fixture 实现完整状态序列 | M15 T116/T117；protocol/Memory/full-chain focused E2E | 5/1/1/5 |
| EF23 | 旧 global remote-Hand 场景与当前 Brain/Hand/Worker publication 并行，形成重复事实来源与无效依赖 | 删除旧 E2E/依赖；ADR 标注历史路径；只保留 lazy Brain/Hand 与 remote Worker 当前场景 | M15 T118；`hand_brain_lazy_e2e.ts`、`hand_brain_remote_worker_e2e.ts` | 4/1/1/4 |

EF11 是唯一仍需外部部署实现才能关闭的高残余项：需要一个真实 provider 同时
实现并证明 Worker-held substitution、目标绑定和不可绕过网络。仓库内不能通过
把 capability 布尔值改成 `true` 来“修复”它；这样只会制造安全虚证。当前的正确
终态是稳定拒绝，G43 因此继续保持 target。

## Workspace crate 逐项 FMECA

“直接”表示 crate 内存在 Rust 测试；“共享”表示 testkit 在拥有真实后端的 crate
中执行。除两个纯 testkit 外，当前 100/102 crates 均有直接测试；公共 API gate
覆盖全部 102 crates。下表列出每个 crate 的主导失效模式，而组合条件仍以上述
M1–M15 判定表为唯一测试设计来源。

### F1–F3：契约、控制面与 Session 编译

| Crate | 主失效模式 → 影响 | 消解/处理与测试证据 | S/O/D/RPN |
|---|---|---|---|
| `awaken-agent-contract` | 状态/事件 wire 漂移或 assistant message ID 无法解析 → 运行恢复误判、后续轮被去重 | 强类型枚举、serde/property；权威 ID prefix/parser 与完整 assistant step 提取测试；直接，M1/M11/M15 | 5/1/1/5 |
| `awaken-credential-contract` | holder/source/envelope 混同 → secret 越界 | exact ref、recipient/expiry/admission；直接，M5 | 5/1/1/5 |
| `awaken-deployment-contract` | deployment revision/capability 不一致 → 错误调度 | 指纹与版本约束、拒绝不兼容；直接，F1 E2E | 4/1/2/8 |
| `awaken-environment-contract` | mutable 环境被重选 → Session 漂移 | immutable revision/fingerprint；直接，M8/M14 | 4/1/1/4 |
| `awaken-environment-realization-contract` | desired/actual 状态混同 → 假 Active | 两阶段状态与 receipt；直接，M14 | 5/1/1/5 |
| `awaken-executable-environment-contract` | 发布序列/快照不闭合 → 旧环境运行 | command sequence/high-water；直接，EF1 | 4/1/2/8 |
| `awaken-provisioning-contract` | capability 降级 → 隔离或 secret 泄漏 | 准入矩阵、合取证据、fail-closed；直接，M8 | 5/1/1/5 |
| `awaken-resource-contract` | 路径/CAS/owner 语义漂移 → 越界或覆盖 | safe path、base hash、owner scope；直接，M13 | 5/1/1/5 |
| `awaken-service-auth-contract` | auth claim/签名语义歧义 → 冒充 | 稳定 claim 和验证错误；直接，M6 | 5/1/1/5 |
| `awaken-session-contract` | root/resource/MCP 多权威 → generation 混用 | 单 root revision、状态机/lease；直接，M14/G42 | 5/1/1/5 |
| `awaken-tenancy` | scope 缺失/冲突 → 跨租户 | opaque `ScopeId`、一致性解析；直接，M6 | 5/1/1/5 |
| `awaken-admin-config-api` | DTO 验证/错误映射错误 → 脏配置或误报成功 | schema 校验、typed error、API snapshot；直接，F1 E2E | 4/1/2/8 |
| `awaken-authz-enforce` | middleware 顺序/默认值错误 → 授权绕过 | authenticate→scope→authorize 串联、默认拒绝；直接，M6 | 5/1/1/5 |
| `awaken-config-resolver` | offering/credential fallback 错误 → 用错模型/secret | exact candidate、禁用端点/冷却判定；直接，M13 | 5/1/1/5 |
| `awaken-config-service` | CRUD 与 executable publication 分叉 → 双来源 | 单应用服务、revalidate 后发布；直接，F1 E2E | 4/1/2/8 |
| `awaken-config-store` | scoped SQL 漏条件/版本覆盖 → 跨域或丢更新 | scope guard、CAS、迁移测试；直接，M13 | 5/1/1/5 |
| `awaken-control` | 组件组装拿错 authority store → 跨角色写 | crate-boundary fitness、显式端口；直接，F1/F2 E2E | 5/1/2/10 |
| `awaken-credential-vault` | 明文持久化/错误 revision → secret 泄漏 | sealed blob、zeroize、exact revision；直接，M5 | 5/1/1/5 |
| `awaken-data-subject` | erase 部分成功或 receipt 丢失 → GDPR 复活 | 内容先删、会计 receipt、erasure fence；直接，M7 | 5/1/1/5 |
| `awaken-environment-application` | authority 已提交但投影失败 → 假发布/永久落后 | commit 保真、失败不伪造 projection、reconcile exact revision；直接判定表，EF1 | 4/1/1/4 |
| `awaken-model-catalog` | offering/dialect/endpoint 漂移 → 无法执行或错路由 | scoped repo、disabled endpoint、fingerprint；直接，M13 | 4/1/1/4 |

### F4、F8：持久化、派工与恢复

| Crate | 主失效模式 → 影响 | 消解/处理与测试证据 | S/O/D/RPN |
|---|---|---|---|
| `awaken-captured-content-store` | 内容与 erasure fence 非原子 → 删除后复活 | fence/receipt 与 scoped backend；直接，M7 | 5/1/1/5 |
| `awaken-env-store` | 环境 command/revision 丢失 → 投影不可恢复 | CAS、command log、SQLite/Postgres conformance；直接，EF1 | 4/1/1/4 |
| `awaken-resource-store` | Resource generation/owner 串线 → 错资源可见 | owner+revision CAS、恢复查询；直接，M14 | 5/1/1/5 |
| `awaken-session-store` | root CAS 或 tombstone receipt 丢失 → 并发覆盖/复活 | SQLite/Postgres 共享 conformance、delete replay；直接，G42 | 5/1/1/5 |
| `awaken-store-conformance` | suite 未挂到后端 → 语义漂移未检出 | 14 项套件由四后端执行，编排器 R8 强制；共享 | 5/1/1/5 |
| `awaken-store-fs` | rename/fsync/并发追加失败 → 部分提交或乱序 | 原子文件发布、锁、共享 14 项 + reopen 测试；直接/共享，M11 | 5/1/2/10 |
| `awaken-store-postgres` | transaction/notify/claim 竞态 → 重复或丢提交 | SQL transaction、CAS、strict Docker PG tests；直接/共享，M10/M11 | 5/1/2/10 |
| `awaken-store-schema` | migration 顺序/方言漂移 → 数据不可读 | versioned migrations、SQLite/Postgres schema tests；直接 | 5/1/2/10 |
| `awaken-store-sqlite` | busy/reopen/transaction 边界错误 → 丢更新 | transaction、CAS、reopen + 共享 14 项；直接/共享，M11 | 5/1/1/5 |
| `awaken-work-store` | claim/settle/erasure 非幂等 → 重复执行或复活 | owner/epoch fence、idempotency receipt；直接，M10 | 5/1/1/5 |
| `awaken-run-ingress-contract` | queue DTO 丢 placement/binding → 错 Worker | typed command/outcome、serde tests；直接，M10 | 5/1/1/5 |
| `awaken-run-ingress-testkit` | backend 未复用队列契约 → 行为分叉 | 同一 suite 跑 memory/sqlite/postgres，编排器 R8 强制；共享 | 5/1/1/5 |
| `awaken-run-ingress` | broad claim 被坏行阻塞、旧 epoch settle 或 durable attempt 丢上层能力 → 饥饿、错提交或策略绕失 | skip incompatible、exact error、atomic epoch fence；继承唯一 attempt context，仅替换 claim authority；直接/共享，M10/M16 | 5/1/1/5 |
| `awaken-coordinator` | 投影落后仍 admission、跨角色 store 写，或 WorkerDirectory 隐式易失 → 陈旧执行/重启丢 incarnation fence | high-water refresh、边界 fitness；显式 durable SQLite/PG Worker authority；无坐标 fail-closed；直接，EF1/EF41 | 6/1/2/12 |
| `awaken-durable-projection` | outbox 重放重复/游标跳跃 → 重复副作用或漏投影 | monotonic cursor、idempotency key、full replay；直接，F1/F10 | 4/1/2/8 |

### F6–F7：Runtime、扩展、资源与 Sandbox

| Crate | 主失效模式 → 影响 | 消解/处理与测试证据 | S/O/D/RPN |
|---|---|---|---|
| `awaken-credential` | refresh/challenge 状态丢失 → 重连失败或错 secret | typed port、exact access；直接，M5 | 5/1/2/10 |
| `awaken-ext-builtin-tools` | 内建工具绕过 permission/commit → 未授权副作用 | 统一 tool descriptor/gate；直接，M4 | 5/1/1/5 |
| `awaken-ext-compact` | 重复/过早压缩 → 上下文丢失 | at-most-once marker、阈值/keep-last；直接，M2 | 3/1/1/3 |
| `awaken-ext-goal` | goal 状态非法跃迁 → 错误终结 | 状态机校验与原子 state command；直接，M3 | 4/1/1/4 |
| `awaken-ext-mcp` | 工具命名冲突/敏感结果泄漏 → 调错工具或泄密 | namespace、逐工具隔离、敏感字段 redaction；直接，M13 | 5/1/1/5 |
| `awaken-ext-memory` | memory scope/path 错误 → 跨会话内容 | Resource port、scope、CAS；直接，M13 | 5/1/1/5 |
| `awaken-ext-permission` | Deny/Suspend 可绕过 → 未授权工具执行 | final gate、Deny precedence、durable await；直接，M4 | 5/1/1/5 |
| `awaken-ext-skills` | 披露被当授权/错误激活 → 工具暴露 | surfaced 与 invocable 分离、glob tests；直接，M13 | 4/1/1/4 |
| `awaken-ext-state-machine` | transition/action 非原子 → 状态与副作用分裂 | compiled transition、state batch validation；直接，M3 | 4/1/1/4 |
| `awaken-mcp-wire` | frame/JSON-RPC id 错配 → 响应串线 | strict decode、correlation、roundtrip；直接，M12 | 4/1/1/4 |
| `awaken-observability` | tracing 失败影响业务、敏感值入日志或 raw URI 造成高基数 | no-op/fail-soft exporter、redaction、`MatchedPath` 模板路由；动态参数路由直接测试 + leak scan，M15 | 4/1/1/4 |
| `awaken-provider-genai` | provider stop/error 映射错误 → 双终结/错误重试 | canonical outcome、breaker/retry 分类；直接，M1 | 4/1/1/4 |
| `awaken-runtime-contract` | port/vocabulary 或 tool registry 并行实现、重复 tool id 按插入顺序覆盖 → 插件绕核心或执行 owner 不确定 | leaf contracts；唯一 `RawToolRegistry` 对 unknown/unique/duplicate fail-closed；crate fitness、API snapshot；直接，M1–M4/M31 | 5/1/1/5 |
| `awaken-runtime` | panic/取消/预算/commit 竞态、resume ID 冲突或 Sandbox 工具回退 Brain → 卡死、错误终态、后续事实丢失或绕隔离 | catch、cancel select、有限预算、单 EndCause；从 committed full assistant steps 派生 resume 起点；target routing 缺 executor fail-closed；直接，M1/M15/M31 | 5/1/1/5 |
| `awaken-store-inmem` | 参考实现与 durable backend 不同 → 错误规格 | 共享 14 项 conformance；直接/共享，M11 | 5/1/1/5 |
| `awaken-tool-pattern` | glob/regex 边界错误 → 工具过授权 | compiled matcher、invalid pattern reject；直接，M4/M13 | 5/1/1/5 |
| `awaken-file-application` | HTTP/Runtime 文件写形成双路径 → 权限/CAS 分叉 | 单 Resources application port；直接，M13 | 5/1/1/5 |
| `awaken-file-store` | path traversal/非原子 publish → 越界或损坏 | safe id、content address、temp+rename；直接，M13 | 5/1/1/5 |
| `awaken-memory-store` | stale CAS/delete 覆盖新 head → 数据丢失 | base sha、conditional delete、idempotent update；直接，M13 | 5/1/1/5 |
| `awaken-resource-application` | activate/release 半成功 → 假 Active 或泄漏 | durable phase、rollback/reconcile；直接，M14 | 5/1/2/10 |
| `awaken-skill-store` | 路径净化/来源 provenance 错 → 越界或伪造技能 | sanitized stem、delivered provenance；直接，M13 | 5/1/1/5 |
| `awaken-environment-image-build` | single-flight/receipt 错 → 重复构建或错镜像 | digest key、claim/lease、exact receipt；直接，F6 E2E | 4/1/2/8 |
| `awaken-environment-package-image-builder` | builder 错误被吞 → 发布不存在镜像 | typed failure propagation、上层状态不晋级；直接，environment E2E | 4/1/2/8 |
| `awaken-sandbox-policy-store` | policy revision 漂移 → 错隔离策略 | exact revision/CAS、scoped store；直接，M8 | 5/1/1/5 |
| `awaken-resource-reclaimer` | cleanup 失败被记成功 → 资源/secret 残留 | Releasing 重试、精确 receipt；直接，M14/F10 E2E | 5/1/2/10 |
| `awaken-connection-plan` | relay/endpoint 选择 fallback → 绕策略 | deterministic plan、unsupported reject；直接，M9 | 5/1/1/5 |
| `awaken-local-process` | env/file secret 生命周期错误 → 子进程泄漏 | typed last-mile、scoped file/env、dispose；直接，M5 | 5/1/1/5 |
| `awaken-sandbox-container` | Docker/Podman/K8s 能力虚报，或 live gate 把宿主运行时故障当产品回归 → 隔离/no-bypass 虚证或测试假红 | provider-specific caps、fail-closed、strict substrate tests；Podman info×raw OCI 独立前提；直接，M8/M32/EF11 | 5/2/2/20 |
| `awaken-sandbox-local` | namespace/FUSE 不可用却降级 → 隔离不足 | capability probe、policy-controlled reject/degrade；直接，M8 | 5/1/1/5 |
| `awaken-sandbox-manager` | lease/pool/reap 竞态 → 复用污染或泄漏 | lease state、poison、shape key、reaper；直接，M9 | 5/1/1/5 |
| `awaken-sandbox-memoryd` | mount/writeback 丢失或越权 → 数据丢失/跨域 | scoped mount token、hash/teardown harvest；直接，M8 | 5/1/2/10 |
| `awaken-tool-relay` | token 重放/撤销后调用，或重复 tool id 选错实现 → 未授权/错误工具执行 | opaque scoped token、lease/revoke；复用唯一 `RawToolRegistry` 且 duplicate ambiguous；直接，M9/M31 | 5/1/1/5 |

### F3、F5、F9–F10：服务应用、协议与 Worker

| Crate | 主失效模式 → 影响 | 消解/处理与测试证据 | S/O/D/RPN |
|---|---|---|---|
| `awaken-acp-application` | ACP request 绕 Session/Run 应用权威 → 状态分叉 | 仅调用 application ports；直接，ACP E2E | 4/1/1/4 |
| `awaken-credential-materializer` | wrong recipient/expired/stale epoch 仍解封 → secret 越界 | exact use-time validation、claim fence；直接，G43-P01–P03 | 5/1/1/5 |
| `awaken-dream-application` | 非法 create 或 cleanup 前外显 terminal → 脏 Session/资源泄漏 | 输入判定表、cleanup committed visibility；直接新增测试，F3/F10 | 5/1/1/5 |
| `awaken-executable-agent-catalog` | 注册替换/重放错序 → 旧 Agent 执行 | monotonic sequence、fingerprint/idempotency；直接，F1 E2E | 4/1/1/4 |
| `awaken-executable-agent-contract` | publication command 漂移 → Control/Coordinator 不兼容 | versioned command/serde；直接，F1 | 4/1/1/4 |
| `awaken-executable-environment-catalog` | delete/update 投影不一致 → 旧环境可选 | sequence/revision/tombstone；直接，F1/F6 | 4/1/1/4 |
| `awaken-managed-bridge` | Managed model/id 转换另建选择逻辑 → 执行候选分叉 | sole ACL/selector bridge；直接，M12/M13 | 4/1/1/4 |
| `awaken-mcp-server-core` | JSON-RPC/transport/HITL 错配 → 请求丢失或误授权 | shared core dispatcher、typed outcomes；直接，M12 | 4/1/1/4 |
| `awaken-mcp-server-testkit` | adapter 未遵守 MCP core 契约 → wire 漂移 | shared adapter suite；直接，protocol MCP tests | 4/1/1/4 |
| `awaken-protocol-a2a` | stop/error/stream 映射不完整 → 客户端误判 | stable mapping；不支持单元明确拒绝；直接，M12 | 4/2/2/16 |
| `awaken-protocol-acp` | fixture newline transport 误入生产/错误映射 → 协议失真 | 生产默认 official ACP；newline 仅 test seam；直接，ACP suites | 5/1/1/5 |
| `awaken-protocol-ag-ui` | 结构化错误退化为字符串 → 客户端不可判别 | typed domain source、当前兼容映射测试；直接，M12 | 3/2/2/12 |
| `awaken-protocol-ai-sdk` | abort/rate-limit/reconnect 映射错 → 错重试 | explicit stop/error/cursor mapping；直接，M12 | 4/1/1/4 |
| `awaken-protocol-awaken` | 原生协议事件序列/游标错 → 重复或漏事件 | canonical event/cursor projection；直接，M12 | 4/1/1/4 |
| `awaken-protocol-managed-resources` | Files/Resource DTO 写错 owner/generation → 越权或泄漏 | auth scope + application port + read-only Files GET；直接，M14 | 5/1/1/5 |
| `awaken-protocol-managed` | archived/write/recovery 语义错 → 复活或错误 2xx | archive fence、typed status、restart tests；直接，M12 | 5/1/1/5 |
| `awaken-protocol-mcp` | adapter 绕 core permission/HITL → 未授权调用 | shared core/testkit、stable reject；直接，M4/M12 | 5/1/1/5 |
| `awaken-run-executor-a2a` | 子进程断线/退出码误映射 → 错 settle | supervised process、typed exit/recovery；直接，M12 | 4/1/2/8 |
| `awaken-run-executor-acp` | ACP 子进程握手/stdio/secret 投影失败，或跨 Run 消息 ID 冲突 → Run 卡住、泄密或后续回复被吞 | official ACP default、typed secret、supervision、Run-scoped fact id；直接，ACP real/deterministic/EF13 | 5/1/1/5 |
| `awaken-runtime-host` | 组装错误、direct/durable attempt context 分叉、Environment executor 缺失或 MCP relay/credential admission 绕过 → 工具不执行、内容不受控或系统级泄漏 | 单 composition root；唯一 attempt-context decorator；SessionEnvironment 唯一提供 Sandbox executor 并复用 `RawToolRegistry`；generation relay、capability conjunction；直接+真实擦除 E2E，M16/M31/G42/G43 | 5/1/2/10 |
| `awaken-webhook-managed` | lifecycle/scope 错配、权威错误被遮蔽，row/secret 跨存储半提交，或旧库存快照误报并发发布 → 漏通知、孤儿/缺失密钥或错误告警 | guard→Workspace；fallible source；唯一 outbox；durable material intent + recovery；库存以首快照界定删除、次快照诊断缺失；直接，M12/M29/M30/M33 | 5/1/1/5 |
| `awaken-webhook` | SSRF、签名错误、永久失败重试风暴、进程重启清空禁用计数或 material ref 泄露签名值 | URL policy、HMAC、唯一 classifier、持久状态回写、opaque random ref；直接+真实 HTTP，M12/M29/M30 | 5/1/1/5 |
| `awaken-worker-registry` | heartbeat/replacement 竞态或重启丢 tombstone → 调度到死 Worker/旧身份历史消失 | incarnation+lease、monotonic replacement、SQLite/PG reopen conformance；Memory 仅 test-support；直接，F5/EF41 | 6/1/1/6 |
| `awaken-worker-transport-security` | 签名重放/错误 trust domain → 冒充 Worker | timestamp/nonce/signature/trust store；直接，F5 | 5/1/1/5 |
| `awaken-acp-contract` | capability fingerprint 非确定或 probe 错误丢信息 → 错兼容判断 | 顺序归一 fingerprint、serde roundtrip；直接新增判定表 | 4/1/1/4 |
| `awaken-agent-channel` | frame 乱序/断线恢复丢 correlation → 响应串线 | sequence/correlation、close semantics；直接，F5 | 5/1/1/5 |
| `awaken-worker-contract` | capability/claim wire 漂移或 Control 获得 mutation port → 不安全 placement/所有权污染 | typed capability + signed envelope；窄只读 `WorkerObservationSource` 与 mutation authority 分离；直接，F5/F6/EF42 | 5/1/1/5 |

### 组合根与开发验证

| Crate | 主失效模式 → 影响 | 消解/处理与测试证据 | S/O/D/RPN |
|---|---|---|---|
| `awaken-cli` | Server 无签名凭据仍启动、AllInOne 未共享聚合，或 split Control 观察自己的空 registry → 未认证 Worker/陈旧 readiness/归档后调度仍存活 | trust fail startup；唯一 `DeploymentState`；唯一 runtime assembly；split Control 复用私有边界轮询 Coordinator 只读观察；直接，EF41–EF43 + schedule E2E | 6/1/2/12 |
| `awaken-sandbox` | CLI 参数绕 capability admission → 低隔离启动 | 所有输入编译为同一 provisioning plan；直接，sandbox E2E | 5/1/1/5 |
| `awaken-worker` | 组装时虚报 provider/credential capability → 错 claim | installed-component-derived caps、lifecycle fence；直接，worker E2E | 5/1/1/5 |
| `awaken-eval` | 评估 fixture 非确定/误计分 → 假回归结论 | deterministic fixture/result tests；直接 | 2/1/1/2 |
| `awaken-runtime-examples` | 示例只注册 Sandbox tool 却不注入 executor，或隐式回退 Brain → 审批后不执行/用户复制绕隔离路径 | `publish=false` + opt-in feature；显式单进程 Hand executor；allow/deny 判定表跟随正式 target-routing API；直接，M31 | 4/2/2/16 |
| `awaken-scenario-host` | 测试 host 与生产组合语义或能力清单分叉 → E2E 假绿/永不 claim | 复用正式 ports/contracts 与生产 A2A 安装器；能力从已安装实现派生；直接，deterministic/remote recovery E2E | 5/1/1/5 |

## 覆盖与使用说明

1. **判定表即测试清单**:每一列(T#)是一个可执行测试用例——置因、驱动被测符号、断言果。列已按 CE 图归约,消除了冗余组合。
2. **约束消除组合爆炸**:O/E 约束(单值枚举)、R 要求边、M 遮蔽优先级共同把理论 2¹⁷¹ 组合压到 190 个有效规则。每个 M(遮蔽)边都配对照用例(如 T52 vs T54、M4 Deny 绝对、M6 三闸串行)专门验证优先级不被违反。
3. **fail-closed 断言**:安全敏感模块(M4/M5/M6/M7/M9)的每个果都应额外断言"默认拒绝/默认封闭"分支——即因全假时落到 fail-closed 果,而非 fail-open。
4. **已知功能空洞**:M12 列出的 A2A 带内拒绝、A2A 流式/推送、AG-UI 结构化错误、ACP 同步 HITL、MCP out-of-band 审批——这些是设计缺口而非 bug,对应用例断言的是"当前遮蔽行为",发现口径改变时须同步更新本表。
5. **多后端等价类**:M10/M11 的后端(fs/sqlite/pg、memory/pg、local/nats/pg-notify)为 O 约束等价类;`MemoryDispatchStore` 是 pg 必须匹配的可执行规格,建议以同一判定表跑参数化后端一致性测试。
