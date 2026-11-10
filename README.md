# Awaken Agents

Build and run AI agent applications on your own infrastructure.

Awaken Agents combines a local Console, Agent publishing, persistent Sessions
and tool permissions. Connect your frontend or backend through supported APIs,
or embed its Rust Runtime. Your application keeps its UI, business rules and
result checks.

[Quickstart](#try-awaken) · [Runnable examples](examples/README.md) ·
[Rust SDK](crates/sdk/runtime/awaken-runtime/README.md) · [Documentation](docs/README.md)

## Why Awaken

- **Keep the same work across connections.** Inspect committed Session history
  after a reconnect or restart; eligible interrupted work follows its recovery policy.
- **Decide what tools may do.** Configure supported tools to allow, deny or wait
  for a human decision, then continue the same work.
- **Choose your application stack and execution.** Use Managed Agents, AI SDK,
  AG-UI, A2A or MCP interfaces, and select Native, supported ACP or remote A2A
  execution according to their documented capabilities.

## Try Awaken

Use this source checkout and its matching dependencies: Rust from
[rust-toolchain.toml](rust-toolchain.toml), Node.js 22+ and pnpm from
[package.json](package.json). Public packages can describe an older release
line; choose documentation matching the version you install.

From the repository root:

```sh
pnpm install --frozen-lockfile
cargo build --locked -p awaken-cli --bin awaken -p awaken-sandbox --bin awaken-sandbox --features awaken-sandbox/hand
./target/debug/awaken database migrate --config examples/application/awaken.toml --initialize-installation --initialization-reference application-example
./target/debug/awaken all-in-one --config examples/application/awaken.toml --no-browser
```

The supplied configuration is for one developer on loopback, in explicit
no-login mode. It retains data in `target/application-data`. Linux Namespace
execution requires usable `bwrap`; other execution tiers and shared deployment
are covered by the [deployment guide](deploy/README.md#local-execution-tiers).
The built executable contains Console and needs no Node.js at runtime.

Open `http://127.0.0.1:8080`. Connect a model provider or supported ACP runtime,
choose **Task assistant** in **Quickstart**, then review, publish and run it.
Copy the published Agent and Environment IDs from **Integrate**.

In another terminal at the repository root:

```sh
export AWAKEN_AGENT_ID=your-published-agent-id
export AWAKEN_ENVIRONMENT_ID=your-environment-id
pnpm --filter @awaken/application-example managed -- "Reply with a two-item API review checklist."
```

You should see a Session ID, the exact accepted input receipt, the reply and
`status=end_turn`. The [application guide](examples/application/README.md)
shows a second message, history readback, approvals and timeout recovery. For a
protected deployment, keep its Workspace service API key in the backend terminal.

<!-- managed-example:start -->
<details>
<summary>View the exact runnable backend example</summary>

This entry is shared with Console. Run it using the commands above; keep
[its support file](examples/application/support.mjs) beside it when copying.

```js
// npm install @anthropic-ai/sdk
// Trusted backend example. Keep the service API key out of browser code.
import Anthropic from '@anthropic-ai/sdk';
// Run inside examples/application; support.mjs handles CLI options and observation.
import { readCommand, observe } from './support.mjs';

try {
  const { values, positionals, timeoutMs } = readCommand();
  const client = new Anthropic({
    apiKey: process.env.AWAKEN_API_KEY ?? 'local',
    baseURL: process.env.AWAKEN_BASE_URL ?? 'http://127.0.0.1:8080',
    maxRetries: 0, // A lost response does not make a mutation safe to repeat.
    timeout: timeoutMs,
  });
  const scope = {
    betas: ['managed-agents-2026-04-01'],
    ...(process.env.AWAKEN_WORKSPACE_ID ? { workspace_id: process.env.AWAKEN_WORKSPACE_ID } : {}),
  };
  if (!values.session && (!process.env.AWAKEN_AGENT_ID || !process.env.AWAKEN_ENVIRONMENT_ID)) {
    throw new Error('Set AWAKEN_AGENT_ID and AWAKEN_ENVIRONMENT_ID from the published Agent and Environment');
  }
  const session = values.session
    ? await client.beta.sessions.retrieve(values.session, scope)
    : await client.beta.sessions.create({
      agent: process.env.AWAKEN_AGENT_ID,
      environment_id: process.env.AWAKEN_ENVIRONMENT_ID,
      ...scope,
    });
  console.log(`session=${session.id}`);
  console.log(`Open in Console: /w/${encodeURIComponent(process.env.AWAKEN_WORKSPACE_ID ?? 'default')}/sessions/${session.id}`);

  if (values.history) {
    for await (const event of client.beta.sessions.events.list(session.id, scope)) {
      if (event.processed_at && event.type === 'agent.message') {
        for (const block of event.content ?? []) if (block.type === 'text') console.log(block.text);
      }
    }
  } else {
    const event = values.approve || values.deny
      ? { type: 'user.tool_confirmation', tool_use_id: values.approve ?? values.deny, result: values.approve ? 'allow' : 'deny' }
      : { type: 'user.message', content: [{ type: 'text', text: positionals.join(' ') || 'Reply with a two-item API review checklist.' }] };
    const receiptId = values.receipt ?? (await client.beta.sessions.events.send(session.id, {
      events: [event], ...scope,
    })).data[0]?.id;
    if (!receiptId) throw new Error('The server returned no Event receipt; inspect the Session before submitting again');
    console.log(`receipt=${receiptId}`);

    await observe(client, session.id, receiptId, scope, timeoutMs);
  }
} catch (error) {
  console.error(`Example failed: ${error.status ? `HTTP ${error.status}; ` : ''}${error.message}`);
  process.exitCode = 1;
}
```
</details>
<!-- managed-example:end -->

## Build with Awaken

| Your next task | Existing example or guide |
|---|---|
| Add a React chat and refresh without losing history | [Browser application](examples/application/README.md#browser-use-the-same-history-through-ai-sdk): a small backend mints the existing short-lived token; the page uses AI SDK |
| Embed execution inside Rust | [Independent SDK consumer](examples/sdk-consumer/README.md): offline examples and executable tests for tools, streaming, approval and restart |
| Work on a repository with approved edits | [Coding TUI](crates/sdk/devtools/awaken-runtime-examples/examples/coding_agent/README.md), or the Console's Repository change starter with a filesystem-capable Environment |
| Produce a sourced research brief | Console's Evidence brief starter with supplied Files, configured search or MCP; inspect sources as well as the answer |
| Explore complete application boundaries | [Reference applications](examples/README.md#full-reference-applications), each with its own source and evidence level |

An Agent defines published behavior; an Environment declares execution needs.
A Session retains work and history, and a Run executes work within it. Backends
use Managed Agents; AI SDK and AG-UI connect application UIs; MCP connects tools;
A2A connects Agents. [Authentication](docs/application-authentication.md) and
[runtime capabilities](docs/design/acp-harness-capability-and-tool-bridge.md)
define their different boundaries.

## More capabilities

Bind [Files, repositories, MemoryStores and Skills](docs/design/resources-memory-files-skills.md),
run [scheduled tasks](docs/design/managed-deployments.md), receive
[lifecycle webhooks](docs/design/managed-deployments.md#recovery-and-webhooks),
and inspect usage with [Session budgets](crates/platform/session/awaken-session-contract/src/budget.rs).
The Runtime also supports [extensions](docs/design/runtime-behavior.md#runtime-extension-surface),
Native [tool state machines](docs/design/tool-state-machine.md) and sub-Agent delegation.

Recovery follows committed facts and configured policy. Reconcile uncertain
external outcomes before retrying; budget admission does not cap every task
cost. Runtime, provider and Environment determine which features are available.
A short prompt or trusted one-off script may need less infrastructure.

## Documentation and contribution

[Documentation](docs/README.md) · [Self-hosting](deploy/README.md) ·
[Contributing](CONTRIBUTING.md) · [Examples and validation](examples/README.md)

Examples, Console snippets and the README code projection share maintained
sources; `pnpm examples:check` detects stale projections and runs sample checks.

For deployment or integration support, see [AwakenWorks](https://awakenworks.com/enterprise?product=agents#apply).

## License

[Apache-2.0](LICENSE). Consult individual package or file metadata for the code
you adopt.
