# ADR-0027: Server-Side Eval History

- **Status**: 📐 Proposed
- **Date**: 2026-05-02
- **Depends on**: ADR-0018, ADR-0023

## Context

`awaken-eval replay` runs fixture-driven evaluation locally and writes one NDJSON
line per fixture to a file on disk
(`crates/awaken-eval/src/report.rs`, `write_ndjson_path`). The output is
consumed by:

- `awaken-eval check`, which diffs two NDJSON files and exits non-zero on
  regression.
- The Eval Reports page in the admin console
  (`apps/admin-console/src/pages/eval-reports-page.tsx`), which today requires
  the user to drag a local NDJSON file into the browser.

Neither path persists reports on the server. Consequences:

1. **No shared baseline.** Two engineers running `awaken-eval replay` against
   the same agent spec produce independent local files that are never
   consolidated. Comparing results across machines requires manual file sharing.
2. **No history.** There is no record of how eval scores changed over time for
   a given agent. Regressions are detectable only within a single `check` run
   against a committed baseline.
3. **CI friction.** CI pipelines write NDJSON to an artifact store, but the
   admin console cannot fetch those artifacts; operators must download and
   drag them in manually.
4. **Eval-server coupling.** `awaken-eval` currently has no dependency on
   `awaken-server`. Introducing a hard link risks coupling the CLI tool to
   server internals; any solution must preserve the option of running eval
   offline.

## Options

### Option A — `awaken-eval` pushes reports to a server endpoint

`awaken-eval replay` gains an optional `--server` flag. When supplied, the
command sends a `POST /v1/eval/reports` request with the NDJSON body after all
fixtures complete (or line-by-line as they finish, for streaming). The server
stores the report and returns a report ID.

**Storage**: a new `eval-reports` namespace in `ConfigStore` (or a dedicated
`EvalReportStore` trait — see below). Each report is stored as a single JSON
document keyed by a ULID.

**Retention policy**: reports older than a configurable window (default 30
days) are pruned by the same background sweep used for audit entries (ADR-0026,
D4). Operators may configure a longer window for compliance purposes.

**Query API for the admin console**:

```
GET /v1/eval/reports
  ?agent_id=<id>       # filter by agent under evaluation
  &since=<RFC 3339>
  &limit=<n>
  &cursor=<opaque>

GET /v1/eval/reports/:id          # full report NDJSON

GET /v1/eval/reports/:id/summary  # aggregated pass/fail counts
```

**Authentication**: `POST /v1/eval/reports` accepts the same admin bearer token
used by `config_routes()`. For CI environments that run eval without admin
credentials, a dedicated eval-runner bearer token may be introduced as a
narrower scope — this is left to a follow-on ADR that extends `AdminApiConfig`.

**Offline operation**: eval continues to write local NDJSON files as it does
today. The `--server` flag is additive; pipelines that do not supply it are
unaffected.

### Option B — Server polls a configured directory

The server watches a directory path (configurable in `ServerConfig`) for new
NDJSON files. When a file appears, it is ingested, stored, and the file is
optionally deleted or moved to an archive subdirectory.

**Storage**: same as Option A.

**Authentication**: no HTTP auth required; access is controlled by filesystem
permissions.

**Offline operation**: fully preserved; eval writes files as today and the
server picks them up.

**Drawbacks**: the server must run on the same host as the eval CLI, or share a
network filesystem. This breaks multi-machine CI setups. File polling is fragile
(partial writes, concurrent runs) and requires careful ordering logic. The
approach is not viable for containerised or distributed deployments.

### Option C — Keep eval-server separation strict; out of scope

Server never stores eval reports. The admin console's drag-and-drop model is
the intended interaction. Teams that want history build their own tooling on top
of CI artifact storage.

**Tradeoff**: zero implementation cost; zero server complexity. The admin
console Eval Reports page remains a local-file viewer. This option is viable as
a long-term position only if eval is treated as a developer-local tool and not
a shared operational dashboard.

## Chosen Path: Option A

Option A preserves the offline-first design of `awaken-eval` while enabling
server-side history. The `--server` flag is purely additive: existing `Makefile`
targets, CI steps, and developer workflows that call `awaken-eval replay` without
`--server` are unaffected. Option B is rejected because it requires filesystem
co-location. Option C is rejected because it forecloses shared baselines and
history, which are the highest-value features of a server-side eval store.

### Storage detail

Reports are stored in the existing `ConfigStore` under the `eval-reports`
namespace. This reuses the file, in-memory, and Postgres backends without a new
storage trait. Each document is:

```json
{
  "id":        "<ulid>",
  "agent_id":  "<string or null>",
  "created_at": "<RFC 3339>",
  "fixture_count": <integer>,
  "pass_count":    <integer>,
  "fail_count":    <integer>,
  "lines":     [ ...ReplayReport objects... ]
}
```

If individual reports grow beyond a practical document size (many thousands of
fixtures), an `EvalReportStore` trait may be introduced to store lines
separately. This decision is deferred; the `ConfigStore` approach is sufficient
for typical fixture counts and can be migrated transparently behind the
`/v1/eval/reports` surface.

### Retention policy

Default: 30 days. Configurable via `AdminApiConfig`. The background sweep
(shared with audit-log retention, ADR-0026, D4) deletes reports beyond the
window. Operators who need longer retention should configure a `ConfigStore`
backend with its own archival policy (e.g., Postgres with a cold-storage export
job).

### Authentication

`POST /v1/eval/reports` is gated by `ensure_admin_auth` alongside the other
config routes. CI pipelines that have the admin bearer token can push reports
directly. A narrower eval-runner credential scope is deferred to a follow-on
ADR that extends `AdminApiConfig` with a secondary bearer token.

## Consequences

- The admin console Eval Reports page can be extended with a "Load from server"
  mode that fetches `GET /v1/eval/reports` without requiring file upload.
  Drag-and-drop remains available for offline use.
- `awaken-eval` gains a new optional dependency on an HTTP client (e.g.,
  `reqwest`) gated behind a feature flag so that builds that do not need server
  push remain lean.
- The `ConfigStore` key space grows with one document per eval run. At typical
  CI cadences (tens of runs per day), the storage overhead is small. The
  retention sweep bounds the total size.
- Teams running `awaken-eval` against a server that does not expose
  `/v1/eval/reports` (e.g., `expose_config_routes = false`, ADR-0023, D2) will
  receive a `404 Not Found` when passing `--server`. The CLI should surface a
  clear error and fall back to writing the local file only.
