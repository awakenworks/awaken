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

Run the fast structural contract before recording:

```sh
pnpm record:test
```

Record against the real console/backend (and supply `KIMI_KEY` for live-model flows):

```sh
pnpm record 06-ai-state-machine
```

Recommended release order:

1. `00-platform-overview` — the short promise and capability-contract proof.
2. `01`–`06` — model supply, agent authoring, policy, resources/trace, AI
   authoring, and State Machine runtime enforcement.
3. `07-runtime-sandbox` — Native/ACP portability plus a persisted sandbox policy.

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
