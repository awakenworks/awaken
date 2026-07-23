# Awaken Console (`web/`)

The management-plane web console (design: `docs/design/web-ui.md`). Two-scope
shell — Workspace supplies (catalog, credentials, MCP, access), Project runs
(sessions, vaults, bindings via the `/projects/{id}` ingress).

```sh
pnpm install
# Production-shaped local run: the console is compiled into the binary.
AWAKEN_HTTP_ADDR=127.0.0.1:38080 \
  cargo run -p awaken-cli --bin awaken -- start

# Frontend hot reload while developing the console:
AWAKEN_HTTP_ADDR=127.0.0.1:38080 cargo run -p awaken-cli --bin awaken &
pnpm dev            # http://127.0.0.1:3002 (proxies /v1)

pnpm typecheck && pnpm lint && pnpm build
AWAKEN_HTTP_URL=http://127.0.0.1:38080 node web/scripts/smoke.mjs  # integration smoke
```

Conventions: `lib/api/client.ts` is the only fetch egress (lint-enforced);
surfaces stay under 600 lines; tokens in `src/styles/tokens.css` (`data-theme`
light/dark); navigation SSOT in `lib/navigation/paths.ts`.
