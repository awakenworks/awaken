# 设计取舍

本页总结 Awaken 几个关键架构决策及其权衡。

## Snapshot Isolation vs Mutable State

**决策**：phase hook 只读不可变 `Snapshot`，写入 `MutationBatch`，所有变更在 gather 结束后原子提交。

**备选**：hook 直接修改共享可变状态。

| | Snapshot Isolation | Mutable State |
|---|---|---|
| 正确性 | 不依赖 hook 执行顺序 | 容易受锁粒度和顺序影响 |
| 并发性 | hook 可安全并行 | 需要精细锁或强制串行 |
| 复杂度 | 需要 batch / merge / conflict machinery | 实现直观 |
| 可调试性 | 每个 phase 边界都是清晰状态跃迁 | 状态变化交织，难追踪 |
| 代价 | 每次 phase 快照需额外 `Arc` clone | 无快照开销 |

**为什么选 Snapshot**：插件组合不应该依赖隐藏顺序。快照模型让 phase 更接近“状态到变更”的纯函数，便于推理和重放。

## Phase-Based Execution vs Event-Driven

**决策**：执行遵循固定 phase 顺序。

**备选**：完全事件驱动，插件异步订阅事件。

| | Phase-Based | Event-Driven |
|---|---|---|
| 可预测性 | 高 | 低 |
| 插件组合 | 边界明确 | 易产生隐式耦合 |
| 可测试性 | 容易对 phase 序列做单测 | 需要模拟异步事件流 |
| 灵活性 | 插入新能力通常要加 phase | 事件扩展自由 |
| 性能 | 顺序 phase 执行引入额外开销 | 可并发处理 |

**为什么选 Phase**：agent 执行天然是“推理 -> 工具 -> 检查终止”的顺序流程。phase 模型更适合在固定节点上插入权限、观察、提醒和请求变换。

## Typed State Keys vs Dynamic State

**决策**：状态键是 Rust 类型，必须实现 `StateKey`。

**备选**：`HashMap<String, Value>` 风格的动态状态。

| | Typed Keys | Dynamic State |
|---|---|---|
| 类型安全 | 编译期保证 | 运行期才发现错误 |
| 合并语义 | 每个键明确声明 `MergeStrategy` | 需要外部约定 |
| 可发现性 | IDE 可跳转 | 只能 grep 字符串 |
| 代价 | 需要定义类型 | 使用方便 |
| 可扩展性 | 新键需要修改代码并重新编译 | 运行时可动态添加新键 |

**为什么选 Typed Keys**：状态错误往往非常隐蔽。把键名、值类型和更新语义统一固化在类型上，更适合作为运行时核心机制。

## Plugin System vs Middleware Chain

**决策**：使用 `PluginRegistrar` 做结构化注册，而不是用 middleware 链包裹整条执行路径。

**备选**：类似 HTTP middleware 的包裹链。

| | Plugin System | Middleware Chain |
|---|---|---|
| 粒度 | 能分别注册 hook / key / tool / effect | 只能包整个执行 |
| 组合 | 多插件可共享同一 phase | 顺序高度敏感 |
| 选择性激活 | 可按 agent 启停插件 | 需要改链结构 |
| 复杂度 | 注册动作较多 | 心智模型简单 |
| 横切关注点 | 天然契合——每个插件处理一个关注点 | 每个 middleware 处理一个关注点，但会看到全部流量 |

**为什么选 Plugin System**：Awaken 的扩展点并不只在单一调用链上，而是散落在 `ToolGate`、phase、state、effects、actions 等多个边界。

## Multi-Protocol Server vs Single Protocol

**决策**：同一个 server 同时提供 AI SDK v6、AG-UI、A2A、MCP HTTP，ACP 则作为独立 stdio 协议模块存在。

**备选**：只支持一种标准协议。

| | Multi-Protocol | Single Protocol |
|---|---|---|
| 前端兼容性 | 可直接接多种生态 | 客户端需自写适配 |
| 维护成本 | 多套 encoder / route | 一套协议面 |
| 测试面 | 更大 | 更小 |
| 运行时耦合 | 运行时保持协议无关 | 容易耦合到唯一协议 |
| 复杂度 | 多套路由、encoder 类型和事件映射 | 一套路由、一套 encoder |

**为什么选 Multi-Protocol**：当前 agent 生态没有统一协议。把协议复杂度压在 server 层，比要求用户自写桥接更实用。

## 另见

- [架构](./architecture.md) -- 三层设计
- [状态与快照模型](./state-and-snapshot-model.md) -- Snapshot Isolation 详解
- [Run 生命周期与 Phases](./run-lifecycle-and-phases.md) -- phase 执行模型
- [Tool 与 Plugin 的边界](./tool-and-plugin-boundary.md) -- plugin 与 tool 的设计边界
