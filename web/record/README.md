# Awaken product-video tests

These scripts are executable product claims, not click macros. A successful run produces
`out/<flow>.mp4`; a failed assertion produces only `out/<flow>.failed.mp4` for diagnosis
and exits non-zero.

Every flow must follow the same story contract:

1. `intro(intent, capability)` — explain the user's desired outcome, then how Awaken
   delivers it, before operating the UI.
2. `checkpoint(name, assertion)` — prove the claim against visible UI or the real API.
   Never swallow a checkpoint timeout.
3. `aha(text)` — end on one concise contrast or payoff that works as a shareable clip.

Each flow also exports one `story` object: `promise`, visible `effect`, exact `aha`,
and the intended `loyalty`, `satisfaction`, and `advocacy` outcome. The harness rejects
multiple Aha moments and enforces a 172-second flow / 180-second final-video ceiling.

Run the fast structural contract before recording:

```sh
pnpm record:test
```

Record against the real console/backend. Live-model flows use the active gcloud
account and require `GEMINI_PROJECT` (optionally `GEMINI_LOCATION` and
`GEMINI_MODEL`):

```sh
AWAKEN_LOCAL_WORKSPACE_ID=wrkspc_default \
  CLOUDSDK_CORE_ACCOUNT=you@example.com \
  AWAKEN_HTTP_ADDR=127.0.0.1:38080 cargo run -p awaken-cli --bin awaken
pnpm dev
GEMINI_PROJECT=my-project pnpm -C web record 01-connect-model
pnpm record 06-ai-state-machine
```

The explicit local workspace id keeps the ephemeral recording backend aligned
with the console's default workspace. Durable installations persist their own id.

Recommended release order (see `VIDEO_STRATEGY.md` for the user-value map):

1. `00-platform-overview` — the short promise and capability-contract proof.
2. `01`–`06` — model supply, agent authoring, policy, resources/trace, AI
   authoring, and State Machine runtime enforcement.
3. `07-runtime-sandbox` — Native/ACP portability plus a persisted sandbox policy.
4. `08-agent-control-plane` — context, compaction, Memory prompts, reminders, and
   completion constraints configured together.
5. `09-protocol-composition` — Managed Agents sessions compose ACP, Vault, and
   direct inline MCP without changing the Agent.
6. `10-skill-optimized-agent` and `11-resource-provenance` — prove that a concise
   request can inherit a Skill and that every delivered Memory/file/Skill remains
   inspectable in the Session.
7. `12-deployment-control` through `14-session-control` — standing Deployment,
   direct Managed Agents API ingress, and an enforced Session archive boundary.
8. `15-a2a-discovery` and `16-access-boundary` — remote Agent Card discovery and
   scoped-token issue/use/revoke proof.
9. `17`–`19` — frontend protocol continuity, MCP server export, and a real Codex
   ACP Agent run whose complete reply returns to the Managed Session transcript.

Release gates are intentionally strict:

- `01`, `02`, and `10` require a working Vertex AI gcloud grant and assert real
  Gemini output. The long-lived grant never enters Awaken.
- `15` requires `A2A_DELEGATE_ID` for a registered, reachable remote delegate.
- `16` requires embedded IAM plus `AWAKEN_RECORD_ADMIN_TOKEN`; the token is injected
  into browser storage and is never rendered in captions or logs.
- `19` requires `AWAKEN_RECORD_CODEX_ACP=1`, `AWAKEN_RECORD_CODEX_ACP_CONTAINER=1`,
  `AWAKEN_ACP_CREDENTIAL_FILE` pointing to an operator-selected mode-`0600` Codex
  `auth.json`, and a backend built with `container-docker` and started with
  `AWAKEN_SANDBOX_TIER=docker`. Networks that require a forward proxy also set
  `AWAKEN_CONTAINER_EGRESS_PROXY`. The runtime checkpoint observes a newly-created
  non-root `awaken.sandbox` container, a writable native credential mount, no API-key
  environment, and one committed live reply; a local or synthetic ACP process is not
  release evidence.
- Dashboard, Eval, Datasets, and Audit remain outside the product series while their
  UI routes are gated. A diagnostic failure artifact is not a release video.

The current repository's `awaken` binary starts the aggregated API/control/runtime
server. It does not embed the Vite console assets, so “run one binary and open the UI”
is not yet an honest installation claim. Record an installation video only after the
release package either embeds the built console or ships a launcher that starts both;
until then, development requires `awaken` plus the console dev/build server.

The harness preflights `BACKEND_URL` (default `http://127.0.0.1:38080`) before
opening the browser, so a stale frontend proxy cannot produce a polished-looking
video of failed API calls.

The harness additionally fails a run that did not execute at least one intro, checkpoint,
and aha. The State Machine flow goes further: it validates the AI-authored machine, opens
a real Sandbox session, attempts a write without a preceding read, and requires the red
tool error plus the State Machine's refusal reason to appear on screen.
