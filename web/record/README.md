# Awaken marketing stories and product proofs

This directory has two deliberately different artifacts:

- `flows/` contains public stories. Each one must give a first-time viewer a
  consequential task, a visible result, a human control point, and a complete handoff. Successful runs create
  `out/<flow>.mp4`.
- `proofs/` contains protocol and integration checks. They exercise the real browser and
  API without recording video. Technical importance alone does not make a marketing story.

`catalog.mjs` is the publication authority. A proof moves back into `flows/` only after its
technical capability supports a human story that is clear without prior product knowledge.
A failed assertion produces a diagnostic artifact and exits non-zero.

Every public story must follow the same contract:

1. `intro(intent, capability)`: explain the user's desired outcome, then how Awaken
   delivers it, before operating the UI.
2. `checkpoint(name, assertion)`: prove the claim against visible UI or the real API.
   Never swallow a checkpoint timeout.
3. `aha(text)`: end on one concise contrast or payoff that works as a shareable clip.

Each flow also exports one `story` object with concrete `stakes`, a
`handoff`, the visible `effect`, and the exact `aha`. The harness rejects
multiple Aha moments. Runtime phases keep hard timeouts so a failed integration cannot
hang the batch, but a complete, well-paced story is not rejected by an arbitrary final
duration ceiling.

Run the fast structural contract before recording:

```sh
pnpm install --frozen-lockfile
npm ci --prefix e2e
pnpm record:test
```

The official Anthropic SDK remains owned by the independently locked `e2e`
package. The batch checks that dependency before recording its first frame, so a
missing clean-install prerequisite cannot leave a partly current series.

Run every product proof, or a selected proof, without creating video:

```sh
pnpm proof:test
pnpm proof:test -- 13-managed-api-ingress
```

Resume every eligible chapter and refresh the evidence manifest with:

```sh
pnpm record:all
pnpm record:all -- 02-build-agent 06-scheduled-operation
```

Existing public MP4s are retained unless `AWAKEN_RECORD_FORCE=1` is set. The release
manifest lists proof-only cases separately and never treats them as missing videos. A
missing proof dependency fails its test with a clear reason; it is not replaced with a
weaker claim or a narrated settings page.

Record against the production console/backend served by one `awaken all-in-one`
process. The harness rejects a split Console/API origin and also checks that the
API listener serves the embedded production Console before it opens the browser.
Live-model stories and proofs use the descriptor-driven Provider Connection UI. Provider,
endpoint, credential, and model catalog are no longer authored as separate recording
steps. Select Vertex with `GEMINI_PROJECT` and an active gcloud account, or select an
API-key provider with `AWAKEN_RECORD_LIVE_PROVIDER` plus its write-only key. The
recorder also recognizes `DEEPSEEK_API_KEY` and `OPENAI_API_KEY` so an existing,
operator-selected recording environment can be reused without copying secrets into
the repository:

```sh
pnpm -C web build
cargo build --release -p awaken-cli --bin awaken
CLOUDSDK_CORE_ACCOUNT=you@example.com target/release/awaken all-in-one \
  --config web/record/recording-all-in-one.toml \
  --data-dir .recording-awaken/data --no-browser --identity-mode no-login
GEMINI_PROJECT=my-project BACKEND_URL=http://127.0.0.1:38080 \
  pnpm -C web proof:test -- 01-connect-model
AWAKEN_RECORD_LIVE_PROVIDER=deepseek DEEPSEEK_API_KEY=... \
  BACKEND_URL=http://127.0.0.1:38080 pnpm -C web proof:test -- 01-connect-model
BACKEND_URL=http://127.0.0.1:38080 CONSOLE_URL=http://127.0.0.1:38080 \
  pnpm -C web record 00-awaken-agents-overview
```

The MCP export is fail-closed and is not mounted unless the all-in-one deployment
configuration contains `mcp_bearer_token`. The CLI deliberately does not discover
deployment credentials from `AWAKEN_MCP_BEARER_TOKEN`. For proof 18, create a
permission-restricted temporary copy of the recording config, write the same local
token into that config, start all-in-one with the temporary file, and pass the token
to the proof as `AWAKEN_RECORD_MCP_TOKEN`. Remove the temporary config after the run;
never commit it or place the token in `recording-all-in-one.toml`.

To reuse a shared release target without duplicating build artifacts, export
`CARGO_TARGET_DIR` during both the build and recording commands. You can instead
set `AWAKEN_RECORD_RELEASE_BINARY` to the exact `awaken` executable. The restart
story uses that same release binary for both process incarnations.

The explicit data root persists the recording Workspace id; the console and backend
therefore use the same authoritative Workspace without an environment override.
If you intentionally use an authenticated local identity mode and the recorder
receives HTTP 401, pass the one-time setup token printed by `awaken all-in-one` as
`AWAKEN_RECORD_SETUP_TOKEN`. The harness exchanges it for the normal HttpOnly browser
session; it never copies the long-lived management service credential into browser storage.

The target public series contains six complementary stories (see
`VIDEO_STRATEGY.md` for the user-value and capability matrix):

| Story | Status | Complete job |
| --- | --- | --- |
| `00-awaken-agents-overview` | executable | Read one controlled evidence packet and return a traceable decision while a person retains approval. |
| `02-build-agent` | executable | Configure, Preview, review, and publish the exact Agent entirely in Console. |
| `03-connect-anthropic-sdk` | executable | Start work through the official Anthropic SDK and inspect the same request and result in Console. |
| `04-human-controlled-action` | executable | Inspect a source snapshot from the current checkout, approve one protected write, then download the reviewed artifact. |
| `05-survive-restart` | executable | Replace the release all-in-one process over one data root, then approve and finish the same pending Session. |
| `06-scheduled-operation` | executable | Capture a real repository snapshot, run an exception-only maintenance brief, and retain a separate Session for the result. |

`TARGET_MARKETING_STORIES` owns this six-story roadmap. `MARKETING_STORIES`
contains only executable scripts. A planned row is not publication evidence, and an
executable script becomes a finished video only after its current source hash, live
checkpoints, subtitles, and quality probe all pass. Each story artifact also records the
exact clean Awaken Git revision used to build and operate the product. A recording from
uncommitted tracked source is rejected before the browser opens.

The following capabilities remain product proofs and do not generate marketing video:

- `01-connect-model` verifies provider authentication, catalog import, and a real model response.
- `03-tools-permissions` verifies that a destructive shell call is denied before execution.
- `04-resources-transparency` verifies cross-Session Memory write, extraction, recall, and trace; any runtime read failure blocks the proof.
- `05-ai-authoring` verifies Assistant drafting and human publication authority.
- `06-ai-state-machine` verifies read-before-write enforcement against a hostile instruction.
- `07-all-in-one-startup` verifies a clean release binary, readiness, API, and embedded Console on one listener.
- `10-skill-optimized-agent` verifies Skill activation through the real model.
- `11-resource-provenance` verifies exact Resource source ids and mount paths in Session Inputs.
- `13-managed-api-ingress`: public-wire Session identity matches the console.
- `14-session-control` verifies interrupt, archive, and rejection of a late write.
- `15-a2a-discovery`: the live public Agent Card matches the console readback.
- `16-access-boundary` verifies issue, use, revoke, and immediate denial of the same key.
- `17-frontend-protocols`: AI SDK and AG-UI share committed history.
- `18-mcp-server-export`: authenticated MCP initialization and explicit tool discovery.
- `19-codex-acp-agent`: real Codex ACP execution returns one committed Session reply.
- `20-tool-presentation` verifies native and deferred MCP tool aliases without adding setup footage to the governance story.

There are intentionally no standalone videos for any case under `proofs/`. Their
mechanisms remain tested, but they do not return to the public series until one named
person receives a useful deliverable, a human control point, and a complete handoff.

Provider API credentials and ACP CLI credentials remain separate:

- Models are connected once on **Providers & models**. Verification stores or
  reuses one credential and imports the catalog for Agent pickers and Assistant.
  There is no Workspace-default model step.
- Claude Code does not run an OAuth login inside ACP. Add its write-only
  `claude setup-token` from **Inference credentials**; it is materialized only
  for `acp:claude`.
- Codex ACP executes the real Codex CLI through its persisted local login and
  trusted host/namespace boundary. The proof verifies live capability,
  visible progress, and one committed reply without opening credential files.

Release gates are intentionally strict:

- Proofs `01`, `03`, `04`, `05`, `06`, and `10`, plus all currently executable stories, require a working live Provider Connection and assert real
  model output. Vertex uses short-lived gcloud OAuth; API-key providers accept a
  write-only secret. Neither secret value is rendered or returned by Awaken.
- Proof `15` reads the real `/.well-known/agent-card.json` route and requires the rendered
  Console JSON to match the live public response. Outbound peers are configured in
  an Agent's Collaboration settings; this page does not invent a global delegate registry.
- `16` requires a self-managed all-in-one host plus its one-time
  `AWAKEN_RECORD_SETUP_TOKEN`. The harness exchanges it for the normal HttpOnly
  browser session. The created service key is tested with a separate, cookie-free
  request before and after revocation, so local admin authority cannot mask the result.
- Proof `19` requires `AWAKEN_RECORD_CODEX_ACP=1`,
  `AWAKEN_RECORD_CODEX_PERSISTED_LOGIN=1`, and a local/namespace all-in-one
  instance whose live capability reports Codex as available. The runtime
  checkpoint observes visible ACP progress and one committed reply from the
  real Codex CLI without reading or copying its persisted credential; a local
  or synthetic ACP process is not
  release evidence.
- Dashboard, Eval, Datasets, and Audit remain outside the product series while their
  UI routes are gated. A diagnostic failure artifact is not a release video.

The `awaken` binary embeds the production Vite console at compile time.
`awaken all-in-one` serves the aggregated API/control/runtime server and the console from
one process and one port, without a web directory, Node.js, or a separate Vite process
at runtime. The installation proof validates one-binary startup only through
`proof:install`: it copies the current binary into a fresh temporary directory,
captures that process's real startup receipt, and opens the Console from the same
listener without generating a video. An already-running development host is not
accepted as installation evidence.

The harness preflights `BACKEND_URL` (default `http://127.0.0.1:38080`) before
opening the browser, so a stale frontend proxy cannot produce a polished-looking
video of failed API calls.

The recording harness fails a public story that did not execute at least one intro,
checkpoint, and aha. The State Machine proof goes further: it validates the AI-authored machine, opens
a real Sandbox session, attempts a write without a preceding read, and requires the red
tool error plus the State Machine's refusal reason to appear on screen.

Every published MP4 must pass the inherited demo-video visual-quality floor: H.264 High,
1920×1200, yuv420p, 30 fps, and CRF 14. Story length is determined by clarity and a
complete value loop, while verified model waits are removed as dead time. Encoding and
`ffprobe` validation happen before the temporary artifact is atomically published.
