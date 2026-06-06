---
title: "Get Started"
description: "Start with the in-process runtime, then add the server control plane when you need shared protocols, managed config, and operations."
---

Use this path if you are new to Awaken and want a local server running quickly:
tools and state live in Rust code, behavior moves through config, and server
mode adds shared protocols plus the browser admin console.

## Start A Local Server

This server works without a model API key. When `OPENAI_API_KEY` is absent, the
starter backend uses a deterministic scripted executor so you can test the HTTP
routes and admin console first.

```sh
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
AWAKEN_ADMIN_API_BEARER_TOKEN=dev-token \
AWAKEN_STORAGE_DIR=./target/awaken-dev \
cargo run -p ai-sdk-starter-agent
```

Check that it is reachable:

```sh
curl -sS \
  -H 'authorization: Bearer dev-token' \
  http://127.0.0.1:38080/v1/capabilities
```

Start the admin console in a second terminal:

```sh
pnpm install
pnpm --filter awaken-admin-console dev
```

Open `http://127.0.0.1:3002`, click the token pill, and paste `dev-token`.
From there you can create a provider, create a model, create an agent, preview
it, and copy the frontend integration route from the saved agent page.

To use a real model from the beginning:

```sh
export OPENAI_API_KEY=<your-key>
export AGENT_MODEL=gpt-4o-mini
# Optional for OpenAI-compatible providers:
export OPENAI_BASE_URL=https://api.openai.com/v1
export OPENAI_ADAPTER=openai
```

Then restart the same `cargo run -p ai-sdk-starter-agent` command. Use
`AWAKEN_SEED_PROFILE=demo` only when you want sample agents and demo tools; the
default `minimal` profile keeps the console focused on the resources you create.

## What You Just Started

- `/v1/ai-sdk/chat` and `/v1/ai-sdk/agents/:agent_id/runs` for AI SDK v6
  frontends.
- `/v1/ag-ui/*` for CopilotKit / AG-UI.
- `/v1/config/*` for managed providers, models, agents, MCP servers, tools,
  and plugin sections.
- `/v1/admin/assistant/*` for the locked Admin Assistant after the first real
  model is configured.
- A file-backed local store under `AWAKEN_STORAGE_DIR`.

## Read in order

1. [Use the Admin Console](/awaken/how-to/use-admin-console/) for the browser
   workflow: configure models, create agents, preview drafts, and publish the
   next runtime snapshot.
2. [Build an Agent with the Admin Assistant](/awaken/how-to/build-an-agent-with-the-assistant/)
   to describe an agent in plain language once a real model is configured.
3. [AI SDK frontend integration](/awaken/how-to/integrate-ai-sdk-frontend/) when
   wiring a React UI to a saved agent.
4. [First Agent](/awaken/tutorials/first-agent/) for the smallest in-process
   runtime.
5. [First Tool](/awaken/tutorials/first-tool/) to understand tool schemas,
   execution, and state writes.
6. [Build an Agent](/awaken/how-to/build-an-agent/) when you want a reusable
   project baseline.

## Leave this path when

- You need to implement new runtime capabilities: go to [Develop Agents](/awaken/build-agents/).
- You need persistence, recovery, or distributed dispatch: go to [State & Storage](/awaken/state-and-storage/).
- You need HTTP, protocol, or frontend integration: go to [Serve & Integrate](/awaken/serve-and-integrate/).
- You need to tune or operate saved agents after those development boundaries are safe: go to [Tune & Operate](/awaken/operate/).
- You are ready to ship: go to [Deploy to Production](/awaken/how-to/deploy-to-production/).
