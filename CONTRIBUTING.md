# Contributing

Thanks for contributing to Tirea.

## Before you start

Use this repo map to find the right place to work:

- `crates/tirea-state/` — state engine, patch apply, conflict detection
- `crates/tirea-state-derive/` — `#[derive(State)]`
- `crates/tirea-contract/` — shared runtime/tool/protocol contracts
- `crates/tirea-agentos/` — agent runtime, inference engine, orchestration, and composition
- `crates/tirea-agentos-server/` — HTTP/SSE/NATS gateway
- `crates/tirea-store-adapters/` — file, memory, postgres, and NATS-backed stores
- `examples/` — frontend starters and example backends
- `docs/book/src/` — tutorials, how-to guides, reference, and explanations

If you are unsure where a change belongs, check the [capability matrix](./docs/book/src/reference/capability-matrix.md) first.

## Development prerequisites

### Required for core Rust work

- Rust toolchain from `rust-toolchain.toml`
- `cargo`
- `lefthook` for local hook validation

### Required for frontend starters

- Node.js 20+
- npm

### Required for docs changes

- `mdbook`
- `mdbook-mermaid`

### Required for some integration and e2e paths

- Docker / Docker Compose for TensorZero and some e2e flows
- Playwright browsers for `e2e/playwright`

## Development setup

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --lib --bins --examples -- -D clippy::correctness
lefthook install
```

## Common commands

### Whole workspace

```bash
cargo build --workspace
cargo test --workspace
cargo clippy --workspace --lib --bins --examples -- -D clippy::correctness
```

### Focus on a single crate

```bash
cargo test -p tirea-state
cargo test -p tirea-agentos
cargo test -p tirea-agentos-server
```

### Frontend starters

```bash
cd examples/ai-sdk-starter && npm install && npm run build
cd examples/copilotkit-starter && npm install && npm run build
```

### Docs

```bash
cargo install mdbook --locked --version 0.5.0
cargo install mdbook-mermaid --locked
bash scripts/build-docs.sh
```

### End-to-end and integration paths

```bash
bash scripts/e2e-playwright.sh
bash scripts/e2e-tensorzero.sh
```

## Coding standards

- Search the repo for existing implementations before adding new code.
- Follow existing crate boundaries and naming conventions.
- Keep behavior deterministic for state/patch operations.
- Prefer explicit error context with `thiserror` + `.context()`.
- Add or adjust tests for every behavior change.
- Update docs only when APIs, schemas, env vars, or architecture-relevant behavior change.

## Pull requests

Before opening a PR, make sure:

- `cargo test --workspace` passes
- `cargo fmt` or the repo format checks pass
- new behavior is covered by tests
- relevant docs are updated when API/schema/config changes
- your PR description explains the user-visible impact and affected crates/examples

For larger changes, open an issue or discussion first so maintainers can confirm direction before you invest heavily.

## Git hooks

This repository uses [lefthook](https://github.com/evilmartians/lefthook) for pre-commit and commit-msg validation.
After cloning:

```bash
lefthook install
```

High-friction validations to know up front:

- root-level markdown files are restricted
- process/status documentation is rejected
- commit messages must follow a strict emoji conventional format
- project-management terms are rejected in docs and commit messages
- placeholder code and common debug statements are flagged

If a hook fails, read the `<system-reminder>` block in the hook output first. The repo includes concrete “Next” steps there.

## Commit messages

This repository uses conventional commit style with emoji:

`<emoji> <type>(<scope>): <subject>`

Rules enforced by lefthook:

- Subject line max 100 characters
- Entire message max 4 lines (subject + blank line + body)
- Body must be separated from subject by a blank line
- No `Co-Authored-By:` or AI generation markers
- No project management terms such as `Phase`, `Sprint`, `Milestone`, `Owner`, or `Assignee`

Example:

```text
✨ feat(agentos): add run cancellation guard
```

## Troubleshooting

### `scripts/build-docs.sh` fails

Make sure both tools are installed:

```bash
cargo install mdbook --locked --version 0.5.0
cargo install mdbook-mermaid --locked
```

### Hooks fail before commit

Common causes:

- commit subject format is invalid
- new docs violate repo doc restrictions
- debug statements or placeholder code were staged
- forbidden root files or directories were added

Run `git diff --cached` and fix the staged content before retrying.

### Example builds fail locally

Check that you are in the correct starter directory and installed dependencies there:

```bash
cd examples/ai-sdk-starter && npm install
cd examples/copilotkit-starter && npm install
```

### Runtime calls fail with provider errors

Match the provider key to the model you are using:

- `tirea-agentos-server` default model: `gpt-4o-mini` → set `OPENAI_API_KEY`
- starter backends default model: `deepseek-chat` → set `DEEPSEEK_API_KEY`
