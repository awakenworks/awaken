---
title: "快速上手"
description: "先从进程内 runtime 开始；需要共享协议、托管配置和运维控制时，再加上 server 控制面。"
---

如果你第一次接触 Awaken，先走这条路径理解核心设计：tools 和 state 留在 Rust
代码里，行为通过配置变化；当同一个 agent 需要共享协议或运维控制时，再加上 server
模式。

## 推荐顺序

1. 阅读 [第一个 Agent](/awaken/zh-cn/tutorials/first-agent/)，先跑通最小 runtime。
2. 阅读 [第一个 Tool](/awaken/zh-cn/tutorials/first-tool/)，理解 tool schema、执行流程和状态写入。
3. 进入 [构建 Agent](/awaken/zh-cn/how-to/build-an-agent/)，把示例整理成可复用的工程基线。
4. 在写生产级工具前，补上 [Tool Trait](/awaken/zh-cn/reference/tool-trait/)。

## 何时离开这条路径

- 需要更多 Agent 能力时，进入 [构建 Agent 路径](/awaken/zh-cn/build-agents/)。
- 需要 HTTP 或前端集成时，进入 [服务与集成](/awaken/zh-cn/serve-and-integrate/)。
- 需要持久化和运行控制时，进入 [状态与存储](/awaken/zh-cn/state-and-storage/) 或 [运行与运维](/awaken/zh-cn/operate/)。
