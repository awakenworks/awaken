# 多智能体模式

Awaken 支持多种 agent 组合方式，包括本地委托、远程 A2A agent、sub-agent 执行以及 handoff。

## 通过 AgentSpec.delegates 进行 agent 委托

agent 可以通过 `delegates` 声明它允许委托的子 agent：

```json
{
  "id": "orchestrator",
  "model": "gpt-4o",
  "system_prompt": "You coordinate tasks across specialized agents.",
  "delegates": ["researcher", "writer", "reviewer"]
}
```

解析时，运行时会为每个 delegate 生成一个 `AgentTool`，LLM 看见的是普通工具，例如 `agent_run_researcher`。

## 通过 A2A 使用远程 agent

如果 `AgentSpec.endpoint` 存在，该 delegate 会被当作远程 A2A agent：

```json
{
  "id": "remote-analyst",
  "model": "unused-for-remote",
  "system_prompt": "",
  "endpoint": {
    "base_url": "https://analyst.example.com",
    "bearer_token": "token-abc",
    "poll_interval_ms": 1000,
    "timeout_ms": 300000
  }
}
```

`A2aBackend` 会发送 `message:send`，读取返回的 `task.id` 并轮询任务状态，再把最终结果包装成 `ToolResult` 返回给父 agent。

## Sub-Agent 模式

### 串行委托

```text
Orchestrator -> researcher -> result
             -> writer     -> result
             -> reviewer   -> result
```

父 agent 按顺序调用子 agent，并根据前一步结果决定下一步。

### 并行委托

如果 LLM 在同一轮推理里一次返回多个 delegate tool call，运行时可并发执行这些子 agent。

### 嵌套委托

```text
orchestrator
  -> team_lead (delegates: [dev_a, dev_b])
       -> dev_a
       -> dev_b
```

每一层都独立通过 `AgentResolver` 解析。理论上没有硬深度限制，但每层都会增加 token 和延迟成本。

## Agent Handoff

handoff 会在同一 run 内把控制权切换给另一个 agent，而不是把它当成子任务调用。

机制：

1. 插件或 handoff 扩展把新 agent ID 写入活动 agent 状态键
2. loop runner 在下一个步骤边界检测到变化
3. 重新通过 `AgentResolver` 解析 agent
4. 在同一个 thread、同一条消息历史上继续执行

### Handoff 与 Delegation 的区别

| 方面 | Delegation | Handoff |
|--------|-----------|---------|
| 控制流 | 父 agent 调子 agent，拿回结果 | 控制权直接切到新 agent |
| Thread 连续性 | 子 agent 可以有独立上下文 | 同一 thread、同一消息历史 |
| 返回路径 | 结果回到父 agent | 不返回，后续由新 agent 接管 |
| 适用场景 | 任务拆解、专长子任务 | 角色切换、升级处理、路由 |

## AgentBackend Trait

本地和远程委托都基于 `AgentBackend`：

```rust,ignore
pub trait AgentBackend: Send + Sync {
    async fn execute(
        &self,
        agent_id: &str,
        messages: Vec<Message>,
        event_sink: Arc<dyn EventSink>,
        parent_run_id: Option<String>,
        parent_tool_call_id: Option<String>,
    ) -> Result<DelegateRunResult, AgentBackendError>;
}
```

这也是实现自定义委托后端的扩展点。

## 另见

- [A2A 协议](../reference/protocols/a2a.md)
- [架构](./architecture.md)
- [Tool 与 Plugin 的边界](./tool-and-plugin-boundary.md)
