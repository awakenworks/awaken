---
title: "Deploy to Production"
description: "Take an awaken-server from a dev binary to a hardened production deployment: build, durable stores, secrets, reverse proxy + TLS, health probes, and admin hardening."
---

This guide ties together the pieces you need to run `awaken-server` in
production. It assumes you already have an agent runtime wired into a server
(see [Serve & Integrate](/awaken/serve-and-integrate/)).

## 1. Build with the features you need

Compile a release binary (or container image) with only the cargo features your
deployment uses — durable stores are feature-gated:

```sh
cargo build --release -p your-server-crate \
  --features "server,postgres,observability,a2a"
```

| Feature | Enables |
|---|---|
| `postgres` | `PostgresStore` + `PgCommitCoordinator` (durable config/thread/run) |
| `nats` | NATS-backed mailbox + buffered thread store for multi-replica |
| `file` | `FileStore` — fine for a single node, not for HA |
| `observability` / `otel` | runtime stats, Prometheus, OpenTelemetry export |
| `permission` | tool-permission HITL |
| `a2a` | Agent-to-Agent backend + routes |

## 2. Configure via environment

| Variable | Purpose |
|---|---|
| `AWAKEN_HTTP_ADDR` | Bind address. Production: `0.0.0.0:<port>` behind a proxy (the `ServerConfig` default is `0.0.0.0:3000`). |
| `AWAKEN_ADMIN_API_BEARER_TOKEN` | Bearer token that protects every config/admin route. **Required** in production. |
| `AWAKEN_ADMIN_CORS_ALLOWED_ORIGINS` | Comma-separated origins allowed to call the admin API from a browser console. |
| `AWAKEN_STORAGE_DIR` | Storage dir for file-based stores (dev/single-node). |
| `AWAKEN_SEED_PROFILE` | Builtin seed (`minimal` / `demo`). Usually unset in prod once you manage config yourself. |
| `AWAKEN_EXPOSE_TRACE_ROUTES` | Exposes trace-read routes. Traces contain prompts/tool args — only expose behind auth. |

Provider credentials are config, not code: set `api_key` on the provider, or run
keyless with `adapter_options.allow_env_credentials = true` and the adapter's env
var (e.g. `VERTEX_API_KEY`). See
[Provider & Model Config](/awaken/reference/provider-model-config/).

## 3. Use durable stores

A dev binary may run on in-memory or file stores; production should not.

- **Config / threads / runs** → Postgres. Wire `PostgresStore` and a
  `PgCommitCoordinator` (all durable runtime writes go through the commit
  coordinator). See [Use Postgres Store](/awaken/how-to/use-postgres-store/).
- **Multi-replica dispatch** → NATS mailbox + buffered thread store. See
  [Use NATS Stores](/awaken/how-to/use-nats-stores/).
- The file/in-memory dev coordinator is refused unless
  `AWAKEN_ALLOW_DEV_FILE_COORDINATOR` is set — do **not** set it in production.

## 4. Manage secrets

- Inject the admin token and provider credentials from your platform's secret
  manager as env vars; never bake them into the image or commit them.
- Prefer keyless providers (`allow_env_credentials`) where the upstream supports
  short-lived env tokens, so no long-lived key sits in config.
- Rotate the admin bearer token and provider keys on a schedule.

## 5. Terminate TLS at a reverse proxy

Run the server on a private interface and put a reverse proxy (nginx, Caddy,
Envoy, or your cloud LB) in front to terminate TLS and forward to
`AWAKEN_HTTP_ADDR`. The server speaks plain HTTP/SSE; keep it off the public
internet directly. Ensure the proxy does **not** buffer SSE responses (disable
proxy buffering on the `/v1/**` streaming routes) or live token streaming will
stall.

## 6. Wire health probes

| Endpoint | Use |
|---|---|
| `GET /health/live` | Liveness — always 200 while the process is up. |
| `GET /health` | Readiness — gates traffic on critical dependencies. |
| `GET /metrics` | Prometheus scrape (with the `observability` feature). |

Point your orchestrator's `livenessProbe` at `/health/live` and `readinessProbe`
at `/health`.

## 7. Observability

Enable the observability plugin and an exporter (Prometheus or OTel) so runtime
stats, latency, and tool/inference metrics are visible — and so the Admin
Console's per-agent stats render. See
[Enable Observability](/awaken/how-to/enable-observability/).

## 8. Harden the admin & config plane

- Keep `AWAKEN_ADMIN_API_BEARER_TOKEN` set and scope CORS to your console origin.
- Serve the [Admin Console](/awaken/how-to/use-admin-console/) behind your edge
  auth; it is a browser client of the same admin API.
- Treat the audit log as your change record — wire it in (retention configured
  separately) so every config write is attributable.

## Checklist

- [ ] Release build with only the needed features
- [ ] Postgres (and NATS for multi-replica) wired; dev coordinator **off**
- [ ] Admin bearer token + scoped CORS set from secrets
- [ ] Provider credentials injected from secrets (or keyless env)
- [ ] TLS terminated at a proxy; SSE buffering disabled
- [ ] Liveness `/health/live` + readiness `/health` probes wired
- [ ] Observability exporter enabled; audit log on

## Related

- [Serve & Integrate](/awaken/serve-and-integrate/)
- [Expose over HTTP/SSE](/awaken/how-to/expose-http-sse/)
- [Use Postgres Store](/awaken/how-to/use-postgres-store/)
- [Use NATS Stores](/awaken/how-to/use-nats-stores/)
- [Tune & Operate](/awaken/operate/)
