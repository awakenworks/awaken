# Coding agent example

A small coding agent assembled from the runtime's own pieces — no new tools, no
new abstractions:

- **Tools**: the built-in hand tools (`read`, `write`, `edit`, `glob`, `grep`,
  `bash`) from `awaken-ext-builtin-tools` — their `ToolDescriptor`s go into the
  `ExecutableAgentSnapshot` (what the model sees), their executables are registered on the
  `Runtime` (what runs). Ids match by construction.
- **Permission** (ADR-0030): `read`/`glob`/`grep` are allowed; `write`/`edit`/
  `bash` are *asked* — each mutation leaves the run `Awaiting` with a `ResumeTicket`, and the
  caller approves (`y`) or denies it before it runs.
- **Config**: `ExecutableAgentSnapshot::builder("coder")…` — built by hand, no config store.
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

- `AWAKEN_MODEL` — the model offered by the selected endpoint. Named MiniMax
  and Kimi configurations have defaults; generic endpoints require it explicitly.
- **MiniMax** (Anthropic-compatible endpoint): set `MINIMAX_API_KEY`, and
  optionally `MINIMAX_BASE_URL` (default `https://api.minimaxi.com/anthropic`).
- **Kimi Code** (OpenAI-compatible endpoint): set `KIMI_API_KEY`, and optionally
  `KIMI_BASE_URL` (default `https://api.kimi.com/coding/v1`); use
  `AWAKEN_MODEL=kimi-k2.7-code`.
- For another OpenAI- or Anthropic-compatible endpoint, set `OPENAI_API_KEY`
  and optional `OPENAI_BASE_URL`, or `ANTHROPIC_API_KEY` and optional
  `ANTHROPIC_BASE_URL`. Set `AWAKEN_MODEL` to a model offered by that endpoint.
  The example does not use an ambient SDK credential fallback.

Provider priority is MiniMax, Kimi, OpenAI, then Anthropic. One configuration
function chooses the credential, endpoint, adapter and model together.

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

For live verification, configure the endpoint above, then run the two file-edit
cases (network and provider access required):

```sh
cargo test --locked -p awaken-runtime-examples --features coding-agent-tui \
  --test coding_agent_minimax -- --ignored \
  --skip minimax_endpoint_authenticates_and_reaches_the_model
```

Despite the historical test filename, these cases use the same selected model
as the TUI. They assert that an approved edit changes the file and a denied edit
leaves it intact. The separate endpoint reachability probe does not prove a task
completed. See [current validation](../../../../../../examples/README.md#validation).
