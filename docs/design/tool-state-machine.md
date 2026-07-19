# Runtime State Machine

State Machine 是 agent 配置中的通用控制机制。内核只认识工具调用、运行阶段事件、状态、数据、计数器和动作；TODO、background task、system reminder 等业务含义由配置和适配器表达。

它只能收紧工具权限，不能绕过 permission 授权。所有状态变更仍通过 runtime 的统一 commit/replay 边界持久化。

## 动态视图

```text
工具调用
  │
  ├─ BeforeTool ── 匹配 on + from + counters
  │                  ├─ 满足：执行工具
  │                  └─ 违反：on_violation = deny | ask | warn
  │                               deny/ask 在执行前拦截
  │
  └─ AfterTool ─── 匹配工具结果 when
                     └─ 原子产生 transition + update + emit

运行阶段
  step.started
      ↓
  step.before_inference ── 产生 request-only context reminder
      ↓                      （当前请求可见，不进入对话历史）
  step.after_inference  ── 清除已消费 reminder；step tick +1
      ↓
  step.ended
```

外部能力（TODO、background task、compaction）不进入内核枚举。适配器把事实转换为同一种事件：

```text
domain fact ── adapter ── { name, data } ── State Machine
```

异步完成消息仍由原有消息通道通知用户；State Machine 只在需要约束、累计状态或决定何时注入 reminder 时消费相应事实，避免重复承担消息投递。

## 静态视图

```text
AgentConfig
└─ state_machine
   ├─ Machine[]
   │  ├─ name / scope / key / initial / terminal
   │  └─ Transition[]
   │     ├─ trigger: ToolPattern | EventPattern
   │     ├─ guard: from + when + counters
   │     ├─ update: state + capture + increment + reset
   │     └─ effect: emit | on_violation
   └─ continuation

MachineInstance = {
  state,
  data: { name: value },
  counters: { name: number }
}

scope = run    → 新 run 不继承
scope = thread → 同一 thread 的后续 run 继续使用
```

模板上下文同时暴露当前事实和持久化实例：`{event.name}`、
`{event.data.*}`、`{instance.state}`、`{instance.data.*}`、
`{instance.counters.*}`。工具参数仍保留在根节点以兼容 `{file_path}` 写法。

DDD 边界：

- `Machine` / `Transition` / `MachineInstance` 是领域模型，不依赖 TODO 或后台任务。
- 纯 evaluator 只产生 `AdvanceOp` 决策，不直接写存储或发送消息。
- Plugin 是应用层适配器，把 runtime hook 转为事件，并把决策转为已有的 State / Conversation 聚合操作。
- Runtime 的 permission、commit、replay 仍是唯一授权和持久化边界。

## Runtime State Machine Role Catalog

| Role / component | Responsibility |
|---|---|
| Machine aggregate | 保存 state/data/counters，并执行配置声明的 transition |
| Pure evaluator | 把工具调用或通用事件计算为 gate / transition / update / emit 决策 |
| State Machine plugin | 将 runtime phase 转为事件，并把决策适配到 State 与 Conversation |
| Fact adapter | 将 TODO、background task、compaction 等领域事实投影为 `{name, data}` |
| Runtime | 负责 permission、hook 顺序、request assembly、commit 和 replay |

## DSL

### Read before write

写操作在工具执行前拦截；一次成功读取后，可以连续写入：

```yaml
machines:
  - name: read-before-write
    scope: thread
    key: "{file_path}"
    key_normalizer: path
    initial: unread
    terminal: [written]
    transitions:
      - on: 'Read(file_path ~ "*")'
        from: [unread, read, written]
        to: read
      - on: 'Write(file_path ~ "*")'
        from: [read, written]
        to: written
        when: { status: success }
        on_violation:
          action: deny
          reason: "Read {file_path} before writing."
```

`on_violation` 属于 BeforeTool；`when`、`to`、`update` 和 transition `emit` 属于 AfterTool，因此失败的写入不会错误推进状态。

### 通用 reminder

```yaml
machines:
  - name: progress-reminder
    scope: thread
    key: ""
    initial: tracking
    transitions:
      - on: { event: step.after_inference }
        from: tracking
        to: tracking
        update:
          increment: [steps_since_management]

      - on: { event: step.before_inference }
        from: tracking
        to: tracking
        counters:
          steps_since_management: { gte: 10 }
        emit:
          target: context
          content: "Review active work before continuing."
          cooldown_steps: 10
        update:
          reset: [steps_since_management]
```

`cooldown_steps` 使用已完成 inference step 的单调计数；它不是消息数或工具结果数。旧字段 `cooldown_turns` 只作为反序列化兼容别名。

### 外部事实适配

TODO 和 background task 可使用相同机制，不增加专用动作：

```yaml
# 适配器事件示例；事件名和 data 都属于配置词汇
on: { event: todo.changed }
update:
  capture:
    snapshot: "{event.data.snapshot}"

on: { event: background.completed }
emit:
  target: context
  content: "Background result is ready: {event.data.summary}"
```

当前 runtime 原生提供四个 step lifecycle 事件。TODO/background/compaction 适配器只需发布同形的 `{name, data}` 事实；纯 `event_evaluate` API 已可处理，业务适配器不应写入 State Machine 内核。

## 持久化与消息语义

| 内容 | scope | 行为 |
|---|---|---|
| Machine instance | `run` / `thread` | 按机器配置持久化 |
| Metrics / violation log | `thread` | 跨 run 审计 |
| Emit throttle | `thread` | cooldown 跨 run 单调 |
| Context reminder | request-only | 推理请求消费后清除，不进入 transcript |
| Conversation emit | conversation | 与工具结果一起提交并回放 |

旧的字符串实例（`machine[key] = "read"`）会无损迁移为 `{state:"read"}`，新增的 `data/counters` 默认为空。

## 验证点

- 写前未读：工具未执行，模型收到明确 violation。
- 读后连续写：两次写均允许，状态保持 `written`。
- 工具失败：不执行仅限成功结果的 transition。
- scope：`thread` 跨 run 保留，`run` 在新 run 清空。
- lifecycle reminder：下一次模型请求可见，commit transcript 不可见，消费后清除。
- cooldown：按 completed step 计算，不受一个 step 内工具数量影响。
- replay：旧实例可迁移，commit 后状态可重建。
