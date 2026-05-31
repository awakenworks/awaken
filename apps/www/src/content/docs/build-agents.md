---
title: "Develop Agents"
description: "Implement executable agent capability in Rust: runtime setup, tools, plugins, state, and controlled sub-agent calls."
---

This path is for the developer side of Awaken: implement the executable
capability that a runtime can safely run. Keep code focused on tools, plugins,
state, providers, stores, and explicit execution boundaries. Move behavior that
operators should change later into managed config, then use
[Tune & Operate](/awaken/operate/) for the browser and REST workflows.

## Recommended order

1. [Build an Agent](/awaken/how-to/build-an-agent/) to define the runtime, model registry, and agent spec.
2. [Add a Tool](/awaken/how-to/add-a-tool/) and [Add a Plugin](/awaken/how-to/add-a-plugin/) to extend behavior safely.
3. [Use Agent Handoff](/awaken/how-to/use-agent-handoff/) when one agent should take over the current thread.
4. [Invoke a Sub-Agent from a Tool](/awaken/how-to/invoke-sub-agent-from-tool/) when custom tool code needs a controlled child run.
5. [Use Generative UI](/awaken/how-to/use-generative-ui/) when an agent should stream UI documents alongside text.
6. [Configure Agent Behavior](/awaken/how-to/configure-agent-behavior/) marks the boundary between code-owned capability and operator-owned tuning.

## Keep nearby

- [Tool Trait](/awaken/reference/tool-trait/) for exact tool contracts.
- [Tool and Plugin Boundary](/awaken/explanation/tool-and-plugin-boundary/) for extension design decisions.
- [Architecture](/awaken/explanation/architecture/) when you need the full runtime model.
