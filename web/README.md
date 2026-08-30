# Awaken Console (`web/`)

The embedded management-plane web console. Product startup is owned by the root
[quickstart](../README.md#try-awaken); this file only describes Console
development.

```sh
pnpm install
# Production-shaped local run: the console is compiled into the binary.
cargo run -p awaken-cli --bin awaken -- all-in-one --port 38080 --no-browser

# Frontend hot reload while developing the console:
cargo run -p awaken-cli --bin awaken -- all-in-one --port 38080 --no-browser &
pnpm dev            # http://127.0.0.1:3002 (proxies /v1)

pnpm typecheck && pnpm lint && pnpm build
AWAKEN_HTTP_URL=http://127.0.0.1:38080 node web/scripts/smoke.mjs  # integration smoke
```

Conventions: `lib/api/client.ts` is the only fetch egress (lint-enforced);
surfaces stay under 600 lines; tokens in `src/styles/tokens.css` (`data-theme`
light/dark); navigation SSOT in `lib/navigation/paths.ts`.
