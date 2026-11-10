# Connect your first application

Create a Session from Node, send a message, then add a React chat that continues
the same conversation. Awaken stores the history; your application owns its UI.

## Before starting

1. Follow the repository [Quickstart](../../README.md#try-awaken) to install,
   build and start Awaken. Use Node.js 22+.
2. In Console, connect a model or supported ACP runtime. Publish a **Task assistant**.
3. Copy its **Agent ID** and **Environment ID** from **Integrate**. A draft cannot run.

## Backend: read a reply, then reuse its Session

In another terminal at the repository root:

```sh
export AWAKEN_AGENT_ID=your-published-agent-id
export AWAKEN_ENVIRONMENT_ID=your-environment-id
pnpm --filter @awaken/application-example managed -- "Reply with a two-item API review checklist."
```

The default connection is `http://127.0.0.1:8080`. For another deployment, set
`AWAKEN_BASE_URL` and its dedicated Workspace service key in `AWAKEN_API_KEY`.
Set `AWAKEN_WORKSPACE_ID` if the credential addresses multiple Workspaces.

Expect output like this; IDs and the answer vary:

```text
session=sesn_...
Open in Console: /w/default/sessions/sesn_...
receipt=evnt_...
…your Agent's checklist…
status=end_turn
```

Save the Session ID and send a follow-up:

```sh
export AWAKEN_SESSION_ID=the-printed-session-id
pnpm --filter @awaken/application-example managed -- --session "$AWAKEN_SESSION_ID" "Expand the second item."
pnpm --filter @awaken/application-example managed -- --session "$AWAKEN_SESSION_ID" --history
```

The second command reads saved replies without starting work. Restart Awaken
with the same configuration and read again to check persistence.

## Browser: use the same history through AI SDK

Keep `AWAKEN_SESSION_ID` and the connection settings in the server's terminal:

```sh
pnpm --filter @awaken/application-example build
pnpm --filter @awaken/application-example start
```

Open `http://127.0.0.1:4317`. Earlier messages appear. Send another message,
then refresh or select **Reconnect and read history**: the same conversation
returns. **Stop** waits for Awaken to acknowledge cancellation. Review pending
tool approvals in Console.

## Change the example

| File | Start here to… |
|---|---|
| [managed.mjs](managed.mjs) | Change the message or see the official SDK's create → send → read flow |
| [src/Chat.tsx](src/Chat.tsx) | Change the React UI and AI SDK transport |
| [server.mjs](server.mjs) | Inspect the backend token exchange and narrow protocol proxy |
| [support.mjs](support.mjs) | Inspect CLI options and bounded observation of the accepted input |

Console and the root README display the same backend source. Keep `support.mjs`
beside it when copying the example. Run `pnpm examples:docs` after changing the
entry; `pnpm examples:check` detects stale README code and checks the example.

The browser receives a short-lived application token, held in memory. Keep the
service key on the backend. This sample has one operator and one configured
Session. For multiple users, authorize each user and map their conversation to
a Session before the same [token exchange](../../docs/application-authentication.md).

## Common problems

| Symptom | Next step |
|---|---|
| HTTP 401/403/404 | Check the connection, Workspace, credentials and published IDs |
| `requires_action` or `budget_reached` (exit 2) | Open the printed Session in Console; review the action or allowance |
| Observation timeout | Use the printed Session and receipt as shown below; the Run may still be active |
| Browser connection fails | Set `AWAKEN_SESSION_ID` in the server's terminal; check that Awaken is running |

<details>
<summary>Observe an accepted input or answer an approval from Node</summary>

```sh
pnpm --filter @awaken/application-example managed -- --session "$AWAKEN_SESSION_ID" --receipt your-printed-receipt-id
pnpm --filter @awaken/application-example managed -- --session "$AWAKEN_SESSION_ID" --approve pending-tool-event-id
# Use --deny instead to refuse the selected tool action.
```

Observation never resends input. `AWAKEN_TIMEOUT_MS` defaults to 120000; expiration
does not cancel the Run. Refusals, transport failures and terminal errors exit 1.
`end_turn` exits 0; inspect the actual answer to decide whether it meets your needs.

</details>

## Verify

```sh
pnpm examples:check
pnpm --filter @awaken/application-example exec playwright install chromium
pnpm --filter @awaken/application-example test:service
```

The service check starts production Awaken and runs these same Node and browser
sources with a deterministic provider fixture. With `DEEPSEEK_API_KEY` supplied
privately, `AWAKEN_EXAMPLE_LIVE=1` selects a real provider instead. See the
[example index](../README.md#validation) for results and remaining limits.
