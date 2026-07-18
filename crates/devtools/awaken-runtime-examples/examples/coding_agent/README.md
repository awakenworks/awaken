# Coding agent example

A small coding agent assembled from the runtime's own pieces — no new tools, no
new abstractions:

- **Tools**: the built-in hand tools (`read`, `write`, `edit`, `glob`, `grep`,
  `bash`) from `awaken-ext-builtin-tools` — their `ToolDescriptor`s go into the
  `RunnableConfig` (what the model sees), their executables are registered on the
  `Runtime` (what runs). Ids match by construction.
- **Permission** (ADR-0030): `read`/`glob`/`grep` are allowed; `write`/`edit`/
  `bash` are *asked* — each mutation leaves the run `Awaiting` with a `ResumeTicket`, and the
  caller approves (`y`) or denies it before it runs.
- **Config**: `RunnableConfig::builder("coder")…` — built by hand, no config store.
- **Turns**: `CodingSession::turn` drives one run and resumes it through each
  approval; turns share a commit coordinator, so the thread's history carries
  across turns.

The reusable core (`src/coding_agent/`) has no TUI and no model SDK. The TUI
(ratatui + crossterm) and the real model (`genai`) live behind the
`coding-agent-tui` feature.

## Run the TUI

```text
cargo run -p awaken-runtime-examples --example coding_agent --features coding-agent-tui
```

Model selection (env):

- `AWAKEN_MODEL` — the model ref (default `MiniMax-M3`).
- **MiniMax** (Anthropic-compatible endpoint): set `MINIMAX_API_KEY`, and
  optionally `MINIMAX_BASE_URL` (default `https://api.minimaxi.com/anthropic`).
- **Kimi Code** (OpenAI-compatible endpoint): set `KIMI_API_KEY`, and optionally
  `KIMI_BASE_URL` (default `https://api.kimi.com/coding/v1`); use
  `AWAKEN_MODEL=kimi-k2.7-code`.
- Otherwise `genai`'s default client reads `OPENAI_API_KEY` / `ANTHROPIC_API_KEY`.

Type a request; the agent reads/searches/edits files under the current directory
and asks before each mutation. `Esc` quits.

## Verify

Offline, deterministic (no key, runs in CI):

```text
cargo test -p awaken-runtime-examples --features coding-agent --test coding_agent
```

A scripted model reads a real temp file, asks to edit it, and the edit is applied
on approval (and not applied on denial) — proving the agent mutates code and that
the permission gate gates mutations.

Live, against a real model (ignored by default; needs network and a funded key).
Verified end to end with Kimi K2.7 — the agent reads the file, edits it (after the
approval prompt), and reports the change:

```text
KIMI_API_KEY=… AWAKEN_MODEL=kimi-k2.7-code \
  cargo test -p awaken-runtime-examples --features coding-agent-tui \
    --test coding_agent_minimax minimax_agent_edits_a_real_file \
    -- --ignored --nocapture
```

`minimax_endpoint_authenticates_and_reaches_the_model` is a lighter check that the
`genai` client routes to the configured model/endpoint (it passes whenever the
endpoint is reachable — e.g. with the MiniMax config, even when its plan quota is
exhausted, a `2056` response proves the wiring is correct).
