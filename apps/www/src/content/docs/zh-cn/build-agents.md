---
title: "开发 Agent 路径"
description: "在 Rust 中实现可执行 Agent 能力：runtime setup、tool、plugin、state 和受控 sub-agent 调用。"
---

这条路径对应 Awaken 的开发侧：实现 runtime 可以安全执行的能力。代码聚焦 tool、
plugin、state、provider、store 和明确执行边界。后续应由运营者调整的行为放进
托管配置，再进入 [调优与运营](/awaken/zh-cn/operate/) 使用浏览器和 REST 工作流。

## 推荐顺序

1. 先读 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)，确定 runtime、model registry 和 agent spec 的骨架。
2. 再读 [添加 Tool](/awaken/zh-cn/how-to/add-a-tool/) 和 [添加 Plugin](/awaken/zh-cn/how-to/add-a-plugin/)，用安全的方式扩展行为。
3. 需要一个 Agent 接管当前 thread 时，阅读 [使用 Agent Handoff](/awaken/zh-cn/how-to/use-agent-handoff/)。
4. 自定义 tool 代码需要受控调用子 Agent 时，阅读 [在工具里调用 Sub-Agent](/awaken/zh-cn/how-to/invoke-sub-agent-from-tool/)。
5. 需要 Agent 在文本之外流式输出 UI 文档时，阅读 [使用 Generative UI](/awaken/zh-cn/how-to/use-generative-ui/)。
6. [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/) 用于理解代码能力和运营调优的边界。

## 建议搭配阅读

- [Tool Trait](/awaken/zh-cn/reference/tool-trait/) 用于核对精确契约。
- [Tool 与 Plugin 的边界](/awaken/zh-cn/explanation/tool-and-plugin-boundary/) 用于判断扩展应该放在哪一层。
- [架构](/awaken/zh-cn/explanation/architecture/) 用于理解完整运行时模型。
