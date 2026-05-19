---
title: "构建 Agent 路径"
description: "当你已经理解基础运行流程，接下来就进入这条路径，把 Agent 能力逐步拼装完整。"
---

当你已经理解基础运行流程，接下来就进入这条路径，把 Agent 能力逐步拼装完整。

## 推荐顺序

1. 先读 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)，确定 runtime、model registry 和 agent spec 的骨架。
2. 再读 [通过配置调优 Agent 行为](/awaken/zh-cn/how-to/configure-agent-behavior/)，通过托管配置调优 provider、model binding、工具和插件 section。
3. 再读 [添加 Tool](/awaken/zh-cn/how-to/add-a-tool/) 和 [添加 Plugin](/awaken/zh-cn/how-to/add-a-plugin/)，用安全的方式扩展行为。
4. 需要发现与委托能力时，继续阅读 [使用 Skills 子系统](/awaken/zh-cn/how-to/use-skills-subsystem/)、[使用 MCP Tools](/awaken/zh-cn/how-to/use-mcp-tools/) 和 [使用 Agent Handoff](/awaken/zh-cn/how-to/use-agent-handoff/)。
5. 需要更具体的能力时，再补上 [使用 Reminder 插件](/awaken/zh-cn/how-to/use-reminder-plugin/)、[使用 Generative UI](/awaken/zh-cn/how-to/use-generative-ui/) 和 [使用延迟加载工具](/awaken/zh-cn/how-to/use-deferred-tools/)。

## 建议搭配阅读

- [Tool Trait](/awaken/zh-cn/reference/tool-trait/) 用于核对精确契约。
- [Tool 与 Plugin 的边界](/awaken/zh-cn/explanation/tool-and-plugin-boundary/) 用于判断扩展应该放在哪一层。
- [架构](/awaken/zh-cn/explanation/architecture/) 用于理解完整运行时模型。
