# Contributing

Thanks for contributing to Awaken.

## Getting Started

### Prerequisites

- **Rust 1.93.0** -- managed via `rust-toolchain.toml` (auto-installed by rustup)
- **lefthook** -- git hooks manager ([install](https://github.com/evilmartians/lefthook/blob/master/docs/install.md))
- An LLM provider API key (OpenAI, Anthropic, DeepSeek, etc.) for running live examples

### Setup

```bash
git clone <repo-url> && cd awaken
lefthook install
cargo build --workspace
cargo test --workspace
```

See [DEVELOPMENT.md](./DEVELOPMENT.md) for full build commands, example runners, and workspace layout.

## Development Workflow

1. Branch from `master`
2. Implement your change incrementally (one logical section at a time)
3. Write tests for each section before moving on
4. Run `cargo test --workspace` and `cargo clippy --workspace --lib --bins --examples --locked -- -D clippy::correctness`
5. Commit using the project's commit convention (see below)
6. Open a pull request against `master`

For larger changes, open an issue first so maintainers can confirm direction before you invest heavily.

## Code Standards

The full coding rules live in [CLAUDE.md](./CLAUDE.md). Key points for contributors:

- **`unsafe_code = "forbid"`** -- no unsafe code anywhere in the workspace
- **Clippy enforcement** -- `cargo clippy` runs on pre-push; all `clippy::correctness` lints are denied
- **Error handling** -- use `thiserror` with project error enums; chain errors with `.context()`
- **Search before implementing** -- check the repo for existing or similar code to avoid duplication
- **No placeholder code** -- no `TODO`, `FIXME`, `HACK`, or stub implementations in commits
- **No hardcoded secrets** -- use environment variables for API keys and tokens

## Commit Convention

This repository uses conventional commit style with emoji, enforced by lefthook:

```
<emoji> <type>(<scope>): <subject>
```

Rules:

- Subject line max 100 characters
- Entire message max 4 lines (subject + blank line + body)
- Body must be separated from subject by a blank line
- No `Co-Authored-By:` or AI generation markers

### Examples

```text
✨ feat(runtime): add phase timeout support
🐛 fix(server): handle SSE disconnect during streaming
♻️ refactor(contract): extract state key validation
📝 docs(adr): add ADR-0019 mailbox architecture
✅ test(permission): add deny policy edge cases
🔧 chore(deps): update genai to 0.6.0-beta.10
```

Common types: `feat`, `fix`, `refactor`, `docs`, `test`, `chore`, `perf`, `style`, `build`, `ci`, `revert`.

## Testing

### Unit and integration tests

```bash
# Full workspace
cargo test --workspace

# Single crate
cargo test --package awaken-runtime
```

Unit tests use `#[cfg(test)]` modules within each crate. Integration tests live in `tests/` directories.

### Live examples

Examples that call LLM providers require API keys in the environment:

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package awaken --example live_test
```

These are run manually and are not part of the standard test suite.

## Pull Request Guidelines

Before opening a PR, verify:

- `cargo test --workspace` passes
- `cargo clippy --workspace --lib --bins --examples --locked -- -D clippy::correctness` passes
- `rustfmt --edition 2024 --check crates/*/src/**/*.rs` passes
- New behavior is covered by tests
- Relevant docs are updated when API, schema, or config changes are involved

A good PR:

- Has a clear description explaining the change and its impact
- Keeps commits atomic and well-scoped
- Does not include unrelated changes
- References related issues when applicable

## Project Structure

The workspace is organized as a core runtime with extension plugins:

| Path | Purpose |
|------|---------|
| `crates/awaken/` | Facade crate (re-exports) |
| `crates/awaken-contract/` | Types, traits, state model |
| `crates/awaken-runtime/` | Execution engine, plugins |
| `crates/awaken-server/` | HTTP server, protocols |
| `crates/awaken-stores/` | Storage backends |
| `crates/awaken-ext-*/` | Extension plugins (permission, MCP, observability, etc.) |
| `examples/` | Full-stack server examples |
| `docs/adr/` | Architecture Decision Records |

See [DEVELOPMENT.md](./DEVELOPMENT.md) for the complete workspace layout, extension authoring guide, and build commands.

## Git Hooks

This repository uses [lefthook](https://github.com/evilmartians/lefthook) for automated validation. After cloning, run `lefthook install`.

Hooks enforce:

- **Pre-commit** -- format checks, forbidden file detection, secret scanning, code quality warnings
- **Pre-push** -- `cargo clippy` with correctness denials
- **Commit-msg** -- emoji conventional format, line limits, prohibited terms

If a hook fails, read the output carefully -- it includes a `Next:` step explaining how to fix the issue.
