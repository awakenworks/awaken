# Development Guide

## Prerequisites

- **Rust 1.93.0** -- managed via `rust-toolchain.toml` (auto-installed by rustup)
- **lefthook** -- git hooks manager ([install](https://github.com/evilmartians/lefthook/blob/master/docs/install.md))
- An LLM provider API key (OpenAI, Anthropic, DeepSeek, etc.) for running examples

## Setup

```bash
git clone https://github.com/AwakenWorks/awaken.git && cd awaken
lefthook install
cargo build --workspace
```

## Building

```bash
# Full workspace
cargo build --workspace

# Single crate
cargo build --package awaken-runtime

# With all features
cargo build --package awaken-agent --features full
```

## Running tests

```bash
# All workspace tests
cargo test --workspace

# Single crate
cargo test --package awaken-runtime

# A specific test
cargo test --package awaken-runtime -- test_name
```

Integration tests that require a live LLM provider are in `crates/awaken/examples/` and are run manually:

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken-agent --example live_test
```

## Running examples

Server examples are in the `examples/` workspace member:

```bash
cargo run --package awaken-examples --example starter_backend
cargo run --package awaken-examples --example travel
```

Core runtime examples are in `crates/awaken/examples/`:

```bash
cargo run --package awaken-agent --example multi_turn
```

All live examples require an LLM provider key in the environment.

## Code style and linting

The workspace enforces:

- **`unsafe_code = "forbid"`** -- no unsafe code anywhere
- **`clippy::all = "warn"`** -- all clippy lints as warnings

Run lints locally:

```bash
# Clippy (same as pre-push hook)
cargo clippy --workspace --lib --bins --examples --locked -- -D clippy::correctness

# Format check
rustfmt --edition 2024 --check crates/*/src/**/*.rs
```

## Git hooks (lefthook)

Hooks are defined in `lefthook.yml` and run automatically:

**Pre-commit:**
- Blocks forbidden root-level files (temp scripts, test data directories)
- Blocks process/status documents (STATUS, REPORT, PROGRESS, etc.)
- Validates documentation content (no project management terms)
- Checks for hardcoded secrets
- Checks for license text in code files
- Runs `rustfmt` on staged `.rs` files (auto-fixes)
- Warns about placeholder code and debug statements

**Pre-push:**
- Runs `cargo clippy` with correctness denials

**Commit message:**
- Enforces format: `<emoji> <type>(<scope>): <subject>`
- Max 100 characters for subject line, max 4 lines total
- Blank line required between subject and body
- Blocks AI generation markers and project management terms

### Commit message examples

```
✨ feat(runtime): add phase timeout support
🐛 fix(server): handle SSE disconnect during streaming
♻️ refactor(contract): extract state key validation
📖 docs(adr): add ADR-0019 mailbox architecture
🧪 test(permission): add deny policy edge cases
🔧 chore(deps): update genai to 0.6.0-beta.10
```

Common type emojis: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `style`, `build`, `ci`, `revert`.

## Adding a new extension

Extensions follow the layout defined in ADR-0013:

```
crates/awaken-ext-xxx/
  src/
    lib.rs              # Public API exports
    plugin/
      mod.rs            # Plugin struct + Plugin trait impl
      hooks.rs          # PhaseHook implementations (if any)
      tests.rs          # Plugin-level tests (#[cfg(test)])
    tools/
      mod.rs            # Tool implementations (or tools.rs if single)
    state.rs            # State key definitions (if any)
    types.rs            # Domain types
    error.rs            # Error types (if any)
```

Key rules:

1. Plugins are self-contained -- register all tools, hooks, and state keys in `Plugin::register()` via `PluginRegistrar`
2. Tools registered through a plugin are scoped to agents that activate that plugin via `plugin_ids`
3. Add the crate to the workspace `Cargo.toml` members list
4. Add it as an optional dependency in `crates/awaken/Cargo.toml` with a corresponding feature flag
5. Use `thiserror` for error types with `.context()` for error chaining

## Workspace structure

```
Cargo.toml                  # Workspace root
crates/
  awaken/                   # Facade crate (re-exports)
  awaken-contract/          # Types, traits, state model
  awaken-runtime/           # Execution engine, plugins
  awaken-server/            # HTTP server, protocols
  awaken-stores/            # Storage backends
  awaken-tool-pattern/      # Tool matching utilities
  awaken-ext-permission/    # Permission plugin
  awaken-ext-observability/ # OpenTelemetry plugin
  awaken-ext-mcp/           # MCP client plugin
  awaken-ext-skills/        # Skills plugin
  awaken-ext-reminder/      # Reminder plugin
  awaken-ext-generative-ui/ # Generative UI plugin
  awaken-ext-deferred-tools/ # Deferred tools plugin
examples/                   # Full-stack server examples
docs/
  adr/                      # Architecture Decision Records
```
