---
title: "Get Started"
description: "Start with the in-process runtime, then add the server control plane when you need shared protocols, managed config, and operations."
---

Use this path if you are new to Awaken and want the core design first: tools
and state live in Rust code, behavior moves through config, and server mode is
added when the same agent needs shared protocols or operator control.

## Read in order

1. [First Agent](/awaken/tutorials/first-agent/) for the smallest runnable runtime.
2. [First Tool](/awaken/tutorials/first-tool/) to understand tool schemas, execution, and state writes.
3. [Build an Agent](/awaken/how-to/build-an-agent/) when you want a reusable project baseline.
4. [Tool Trait](/awaken/reference/tool-trait/) before writing production tools.

## Leave this path when

- You need more agent capabilities: go to [Build Agents](/awaken/build-agents/).
- You need HTTP or frontend integration: go to [Serve & Integrate](/awaken/serve-and-integrate/).
- You need persistence or operational controls: go to [State & Storage](/awaken/state-and-storage/) or [Operate](/awaken/operate/).
