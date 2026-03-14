> 本文档为中文翻译版本，英文原版请参阅 [Architecture](../../explanation/architecture.md)

# 架构

Tirea 运行时由三个层次组成：

```text
Application -> AgentOs (orchestration + execution engine) -> Thread/State Engine
```

## 1. 应用层

你的应用程序负责定义工具、智能体定义及集成端点。

主要调用路径：

- 通过 `AgentOsBuilder` 构建 `AgentOs`
- 提交 `RunRequest`
- 消费流式 `AgentEvent`

## 2. AgentOs（编排 + 执行）

`AgentOs` 同时负责运行前的编排与循环执行：

**编排**（`composition/`、`runtime/`）：
- 解析智能体/模型/插件的连接关系（插件实现 `AgentBehavior` 特征）
- 加载或创建线程
- 对传入消息去重
- 持久化运行前检查点
- 构建 `RunContext`

**执行引擎**（`engine/`、`runtime/loop_runner/`）：

循环由阶段驱动：

- `RunStart`
- `StepStart -> BeforeInference -> AfterInference -> BeforeToolExecute -> AfterToolExecute -> StepEnd`
- `RunEnd`

终止条件在 `RunFinish.termination` 中显式指定。

## 3. Thread + State 引擎

状态变更基于补丁（patch）机制：

- `State' = apply_patch(State, Patch)`
- `Thread` 存储基础状态、补丁历史及消息
- `RunContext` 累积运行增量，并通过 `take_delta()` 触发持久化

## 设计意图

- 确定性状态转换
- 追加式持久化，配合版本校验
- 与传输层无关的运行时（以 `AgentEvent` 作为核心流）

## 参见

- [运行生命周期与阶段](./run-lifecycle-and-phases.md)
- [前端交互与审批模型](./frontend-interaction-and-approval-model.md)
- [持久化与版本控制](./persistence-and-versioning.md)
- [HTTP API](../reference/http-api.md)
- [事件](../reference/events.md)
