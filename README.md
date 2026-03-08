# Tirea

Tirea is an immutable state-driven agent framework in Rust.
It combines typed JSON state management, deterministic patch application, and an agent runtime/orchestration stack.

## Documentation

- Published docs: <https://tirea-ai.github.io/tirea/>
- Book entrypoint in repo: [`docs/book/src/introduction.md`](./docs/book/src/introduction.md)
- API reference guide: [`docs/book/src/reference/api.md`](./docs/book/src/reference/api.md)

## Start here

Choose the path that matches what you want to do first:

- **Run a full-stack demo quickly**
  - [AI SDK starter](./examples/ai-sdk-starter/README.md) — Vite + React + AI SDK v6
  - [CopilotKit starter](./examples/copilotkit-starter/README.md) — Next.js + AG-UI + CopilotKit
- **Learn the core runtime APIs**
  - [First Agent tutorial](./docs/book/src/tutorials/first-agent.md)
  - [First Tool tutorial](./docs/book/src/tutorials/first-tool.md)
- **Integrate the Rust crates into your own app**
  - [Build an Agent](./docs/book/src/how-to/build-an-agent.md)
  - [Expose HTTP SSE](./docs/book/src/how-to/expose-http-sse.md)
- **Contribute to the framework itself**
  - [Contributing guide](./CONTRIBUTING.md)
  - [Capability matrix](./docs/book/src/reference/capability-matrix.md)

## Recommended newcomer paths

| Goal | Start with | Then go to |
|---|---|---|
| Build your first backend agent | [`docs/book/src/tutorials/first-agent.md`](./docs/book/src/tutorials/first-agent.md) | [`docs/book/src/how-to/build-an-agent.md`](./docs/book/src/how-to/build-an-agent.md) |
| See a working frontend + backend app | [`examples/ai-sdk-starter/README.md`](./examples/ai-sdk-starter/README.md) | [`examples/copilotkit-starter/README.md`](./examples/copilotkit-starter/README.md) |
| Understand the public API surface | [`docs/book/src/reference/api.md`](./docs/book/src/reference/api.md) | `cargo doc --workspace --no-deps --open` |
| Work on core runtime/orchestration code | [`CONTRIBUTING.md`](./CONTRIBUTING.md) | [`docs/book/src/reference/capability-matrix.md`](./docs/book/src/reference/capability-matrix.md) |

## Repository map

### Core workspace crates

- `tirea-state`: typed state + JSON patch apply/conflict detection
- `tirea-state-derive`: `#[derive(State)]` proc macro
- `tirea-contract`: shared runtime/tool/protocol contracts
- `tirea-agent-loop`: agent execution loop
- `tirea-agentos`: orchestration and composition
- `tirea-store-adapters`: memory/file/postgres/nats-buffered stores
- `tirea-agentos-server`: HTTP/SSE/NATS gateway
- `tirea`: umbrella re-export crate

### Examples and integration surfaces

- `examples/ai-sdk-starter/`: AI SDK v6 starter, fastest browser-based demo path
- `examples/copilotkit-starter/`: CopilotKit + AG-UI starter
- `examples/travel-ui/`: travel planning UI demo, see [`examples/travel-ui/README.md`](./examples/travel-ui/README.md)
- `examples/research-ui/`: research workflow UI demo, see [`examples/research-ui/README.md`](./examples/research-ui/README.md)
- `examples/src/`: shared Rust example backends and tools
- `e2e/`: Playwright, Phoenix, and TensorZero integration tests

## Quick start

### Prerequisites

- Rust toolchain from [`rust-toolchain.toml`](./rust-toolchain.toml)
- For frontend starters: Node.js 20+ and npm
- One model provider key for the path you are running

### Build and test the Rust workspace

```bash
cargo build --workspace
cargo test --workspace --locked
cargo clippy --workspace --lib --bins --examples --locked -- -D clippy::correctness
```

### Run the default server configuration

The default `tirea-agentos-server` model is `gpt-4o-mini`, so the simplest matching quick start is:

```bash
export OPENAI_API_KEY=<your-key>
cargo run --package tirea-agentos-server -- --http-addr 127.0.0.1:8080
```

If you want a DeepSeek-first local demo instead, use one of the starters below. Their backend defaults are already aligned to `deepseek-chat`:

- [`examples/ai-sdk-starter/README.md`](./examples/ai-sdk-starter/README.md)
- [`examples/copilotkit-starter/README.md`](./examples/copilotkit-starter/README.md)

## Examples

### AI SDK starter

Use this when you want the shortest path to a browser demo with a Rust backend.

```bash
cd examples/ai-sdk-starter
npm install
DEEPSEEK_API_KEY=<your-key> npm run dev
```

See: [`examples/ai-sdk-starter/README.md`](./examples/ai-sdk-starter/README.md)

### CopilotKit starter

Use this when you want AG-UI + CopilotKit integration with persisted threads and canvas flows.

```bash
cd examples/copilotkit-starter
npm install
cp .env.example .env.local
export DEEPSEEK_API_KEY=<your-key>
npm run setup:agent
npm run dev
```

See: [`examples/copilotkit-starter/README.md`](./examples/copilotkit-starter/README.md)

### Travel UI

Use this when you want a smaller CopilotKit + AG-UI scenario demo focused on map actions and approval-gated trip planning.

See: [`examples/travel-ui/README.md`](./examples/travel-ui/README.md)

### Research UI

Use this when you want a CopilotKit + AG-UI scenario demo focused on resource collection, report writing, and approval-gated deletion.

See: [`examples/research-ui/README.md`](./examples/research-ui/README.md)

## Build docs locally

- Book source: `docs/book/src/`

```bash
cargo install mdbook --locked --version 0.5.2
cargo install mdbook-mermaid --locked
bash scripts/build-docs.sh
```

Then open `target/book/index.html`.

Publish docs from CI:

- The `Docs` workflow always builds the book and Rust API docs on `main`.
- GitHub Pages deployment requires the repository Pages source to be set to `GitHub Actions`.
- GitHub Pages deployment is gated by the repository variable `ENABLE_GITHUB_PAGES_DOCS=true`.
- The published site is available at <https://tirea-ai.github.io/tirea/>.

## Project source

The canonical repository is:

- https://github.com/tirea-ai/tirea

## Security and contributing

- Security policy: [`SECURITY.md`](./SECURITY.md)
- Contributing guide: [`CONTRIBUTING.md`](./CONTRIBUTING.md)
- Code of conduct: [`CODE_OF_CONDUCT.md`](./CODE_OF_CONDUCT.md)

## License

Dual-licensed under:

- MIT ([`LICENSE-MIT`](./LICENSE-MIT))
- Apache-2.0 ([`LICENSE-APACHE`](./LICENSE-APACHE))
