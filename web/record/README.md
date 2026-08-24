# Awaken product-video tests

These scripts are executable product claims, not click macros. A successful run atomically
produces `out/<flow>.mp4`; a failed assertion produces `out/<flow>.failed.mp4` plus a
diagnostic screenshot and exits non-zero. If FFmpeg itself fails, the harness exits non-zero
and keeps `out/<flow>.failed.webm` instead of publishing a partial or stale MP4. Set
`AWAKEN_RECORD_KEEP_WEBM=1` only when the raw successful recording is needed for editing.

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

Record against the real console/backend. The live model story must use the
descriptor-driven Provider Connection UI—provider, endpoint, credential, and
model catalog are no longer authored as separate recording steps. It uses the
active gcloud account and requires `GEMINI_PROJECT` (optionally
`GEMINI_LOCATION` and `GEMINI_MODEL`):

```sh
mkdir -p .recording-awaken
printf 'data_dir = ".recording-awaken/data"\nbind = "127.0.0.1:38080"\n' \
  > .recording-awaken/config.toml
CLOUDSDK_CORE_ACCOUNT=you@example.com cargo run -p awaken-cli --bin awaken \
  -- all-in-one --config .recording-awaken/config.toml
pnpm dev
AWAKEN_RECORD_SETUP_TOKEN=the-one-time-token \
  GEMINI_PROJECT=my-project pnpm -C web record 01-connect-model
pnpm record 06-ai-state-machine
```

The explicit data root persists the recording Workspace id; the console and backend
therefore use the same authoritative Workspace without an environment override.
For a fresh local data root, pass the one-time setup token printed by `awaken serve`.
The harness exchanges it for the normal HttpOnly browser session; it never copies the
long-lived management service credential into browser storage.

Recommended release order (see `VIDEO_STRATEGY.md` for the user-value map):

1. `00-platform-overview` — the short promise and capability-contract proof.
2. `01`–`06` — model supply, agent authoring, policy, resources/trace, AI
   authoring, and State Machine runtime enforcement.
3. `10-skill-optimized-agent` and `11-resource-provenance` — prove that a concise
   request can inherit a Skill and that every delivered Memory/file/Skill remains
   inspectable in the Session.
4. `12-deployment-control` through `14-session-control` — standing Deployment,
   direct Managed Agents API ingress, and an enforced Session archive boundary.
5. `15-a2a-discovery` and `16-access-boundary` — remote Agent Card discovery and
   scoped-token issue/use/revoke proof.
6. `17`–`19` — frontend protocol continuity, MCP server export, and a real Codex
   ACP Agent run whose complete reply returns to the Managed Session transcript.

There are intentionally no standalone `08` or `09` release videos. Both ended
at configuration or metadata rather than an executed user effect. Behavior is
proven by the Memory and State Machine runtime stories; protocol composition is
proven by the MCP runtime and real Codex ACP stories.

Provider API credentials and ACP CLI credentials remain separate:

- Models are connected once on **Providers & models**. Verification stores or
  reuses one credential and imports the catalog for Agent pickers and Assistant.
  There is no Workspace-default model step.
- Claude Code does not run an OAuth login inside ACP. Add its write-only
  `claude setup-token` from **Inference credentials**; it is materialized only
  for `acp:claude`.
- Codex ACP uses its native operator-selected `auth.json`; it is not a Provider
  API key and does not make catalog models ready.

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
  `AWAKEN_CONTAINER_FORWARD_PROXY`. This is connectivity configuration, not an
  allowlist boundary. The runtime checkpoint observes a newly-created
  non-root `awaken.sandbox` container, a writable native credential mount, no API-key
  environment, and one committed live reply; a local or synthetic ACP process is not
  release evidence.
- Dashboard, Eval, Datasets, and Audit remain outside the product series while their
  UI routes are gated. A diagnostic failure artifact is not a release video.

The `awaken` binary embeds the production Vite console at compile time.
`awaken all-in-one` serves the aggregated API/control/runtime server and the console from
one process and one port, without a web directory, Node.js, or a separate Vite process
at runtime. An installation video may claim one-binary startup when its checkpoint
runs a release binary from a clean directory and opens the console successfully.

The harness preflights `BACKEND_URL` (default `http://127.0.0.1:38080`) before
opening the browser, so a stale frontend proxy cannot produce a polished-looking
video of failed API calls.

The harness additionally fails a run that did not execute at least one intro, checkpoint,
and aha. The State Machine flow goes further: it validates the AI-authored machine, opens
a real Sandbox session, attempts a write without a preceding read, and requires the red
tool error plus the State Machine's refusal reason to appear on screen.
