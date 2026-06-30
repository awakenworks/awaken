# Coding agent example

A small coding agent assembled from the runtime's own pieces — no new tools, no
new abstractions:

- **Tools**: the built-in hand tools (`read`, `write`, `edit`, `glob`, `grep`,
  `bash`) from `awaken-ext-builtin-tools` — their `ToolDescriptor`s go into the
  `RunnableConfig` (what the model sees), their executables are registered on the
  `Runtime` (what runs). Ids match by construction.
- **Permission** (ADR-0030): `read`/`glob`/`grep` are allowed; `write`/`edit`/
  `bash` are *asked* — each mutation parks the run on a `WaitingTicket`, and the
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

Live, against a real model (ignored by default; needs a funded key + network):

```text
MINIMAX_API_KEY=… AWAKEN_MODEL=MiniMax-M3 \
  cargo test -p awaken-runtime-examples --features coding-agent-tui \
    --test coding_agent_minimax -- --ignored --nocapture
```
