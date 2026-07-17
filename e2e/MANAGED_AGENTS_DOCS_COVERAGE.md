# Managed Agents — Docs-Driven e2e Coverage

Test-coverage design and gap analysis for the **Claude Managed Agents** API
(`platform.claude.com/docs/en/managed-agents/*`), mapped against awaken's TS/Node
e2e conformance suite in `e2e/` (~170 `*_e2e.mjs` suites + the static conformance
gate in `e2e/conformance/`).

The oracle is the installed official SDK (`@anthropic-ai/sdk`, pinned in
`e2e/package.json`); the subject under test is awaken's Rust wire vocabulary. The
static gate (`npm run test:conformance`) already pins awaken's event catalog +
`MANAGED_BETA` + the serde golden to that SDK — the audit below is about *behavioral*
coverage on top of that structural conformance.

## Method

Every documented page was decomposed into concrete testable behaviors, then
cross-referenced against the actual suites. Test-design methods applied per surface:
equivalence partitioning (network policies, package managers, rubric kinds), boundary
values (page `limit`, compaction window, `max_iterations`, 100k spill), state-transition
coverage (session status, deployment lifecycle, multiagent thread lifecycle, outcome
result machine), and error-guessing (fabricated cursors, unparseable cron, unreachable
MCP servers).

Each gap is classed by **implementation status in awaken**, because a doc behavior with
no test is not automatically a coverage hole — awaken is a self-hostable conformant core,
so some doc surface is Anthropic-cloud-only or a research preview it does not implement.

- **✅ Closed this pass** — a real, implemented-but-untested behavior now covered.
- **○ Implemented, untested** — awaken implements it; a future suite would pass. Backlog.
- **▲ Not implemented / out-of-scope** — the *reason* the code is uncovered: awaken does
  not implement this doc surface (cloud-only, infra-level, or research preview). Not a
  bug, not dead code.

## Closed this pass

| Behavior | Doc page | New suite | Design |
|---|---|---|---|
| Session-list cursor pagination (`?limit=&page=`, `{data,has_more,next_page}`, after-id) | session-operations | `managed_session_pagination_e2e.mjs` | boundary (`limit=1` vs all) + state-transition on the cursor + error-guess (fabricated cursor → empty terminal page) |
| Outcome evaluation lifecycle spans (`span.outcome_evaluation_start`/`_end` bracket every iteration; stable `outcome_id`; monotonic `iteration`) | define-outcomes / reference | `managed_outcome_lifecycle_e2e.mjs` | state-transition: each `_start(outcome_id,iteration)` pairs with one `_end` at the same key; iterations contiguous ascending |
| Scheduled deployments (cron `schedule` echo/persist, pause retains, write-time cron validation) | scheduled-deployments | `management_deployment_schedule_e2e.mjs` | equivalence partition on cron expr (valid / garbage / missing → 400) + state-transition pause→unpause preserves schedule |

Verified against source before writing: `deployments.rs` (`projected_schedule`/`active_cron` +
write-time `Cron::parse` 400), `cron.rs` (dependency-free 5-field evaluator),
`types/page.rs` (`paginate` after-id cursor), `types/session.rs`
(`SpanOutcomeEvaluationStart{outcome_id,iteration}`). All three suites are registered in
`package.json` (`test` + `test:extended`) and pass.

## Implemented, untested (backlog — ranked)

These are real coverage gaps: awaken implements the behavior, no e2e asserts it.

1. **MCP tool confirmation (`always_ask`) approve/deny** — the MCP toolset defaults to
   `always_ask`, but every MCP suite auto-runs (`always_allow`). No test parks an
   `agent.mcp_tool_use` at `requires_action` and resolves it via `user.tool_confirmation`.
   → new `managed_mcp_hitl_e2e.mjs` (decision table {mcp tool × allow|deny} × fixture side-effect).
2. **MCP `session.error` classification** (`mcp_connection_failed_error` /
   `mcp_authentication_failed_error` + `mcp_server_name` + `retry_status`; session still
   starts) → new `managed_mcp_error_e2e.mjs`.
3. **Mid-run interrupt + steer** — only the no-op interrupt (no active run) is tested.
   Interrupting an actively-running turn and redirecting is uncovered.
   → `managed_interrupt_steer_e2e.mjs`.
4. **Session agent-update gate** — only `tools`/`mcp_servers` mutable; full-array-replacement
   semantics, running-session → refuse, `model`/`system` mutation → reject. Untested.
5. **Overrides clearing rules** — `system:null` clears; `tools` cleared with non-empty skills → 400.
   Only `model:null`→400 covered.
6. **`agent.thinking` start-only preview + no-replay-on-reconnect** — previews best-effort.
7. **Deployment run failure taxonomy + auto-pause/auto-archive** (`environment_archived_error`,
   `agent_archived_error`, `session_rate_limited_error`; `has_error` run filter).
8. **`limited` networking sub-flags** — `allow_mcp_servers` / `allow_package_managers`
   (default `false`); only `allowed_hosts` is exercised.
9. **Worker lease-reclaim** — an un-acked lease past `reclaim_older_than_ms` becomes re-claimable
   (queue-level; no real CLI needed). → `management_worker_lease_reclaim_e2e.mjs`.
10. **`read_only` memory mount rejects writes** (fs-level enforcement; container tier verified in
    `docker.rs`) — only `read_write` mounts are tested.
11. **`mcp_toolset` declaration + tool filtering** — no test puts an `mcp_toolset` entry in `tools`
    or exercises `default_config.enabled:false` enable-lists on MCP tools.
12. **Multiagent thread control** — `{type:"self"}` coordinator self-copy; interrupt a
    `requires_action` child (denies pending tools, re-idles `end_turn`, no sampling); archive
    rejected unless idle. Delegation *happy path* is covered.
13. **Files negatives** — `downloadable:false` on uploaded files, download-of-uploaded → 400,
    invalid-filename → 400, `document`/`image` `file_id` content block in a turn.
14. **`session.status_rescheduled` / `rescheduling`** — implemented (transient-retry counting),
    but hard to drive deterministically from the current fake-upstream fault modes; needs a
    once-transient scenario mode. Lower priority.

## Not implemented / out-of-scope — *why the code is uncovered*

These doc behaviors have **no awaken implementation**, verified by source search. That is
the reason they are uncovered; they are not dead or redundant code, and adding tests would
assert against absent features.

| Doc surface | Status in awaken | Evidence |
|---|---|---|
| **Dreams** (`/v1/dreams`, `dreaming-2026-04-21` header, create/poll/cancel/archive) | Not implemented — research preview | no `dreams`/`Dream` route or type in `crates/` |
| **`agent-memory-2026-07-22` endpoint header + two-header 400 conflict** | Not implemented — awaken keys memory-store endpoints off the same managed beta | no `agent-memory-2026-07-22` string in `crates/` |
| **`system.message` 1–1000 content-item boundary + `model_does_not_support_mid_conversation_system` 400** | Not implemented — accept/reject-before-first-turn is covered; no item-count or model-capability gate | no count validation in `routes/`/`types/session.rs` |
| **Cloud env `packages` provisioning** (pip/npm/apt/cargo/gem/go, version pinning) | Out of scope — Anthropic-managed cloud sandbox only; awaken's sandbox does not parameterize packages | `managed_environment_e2e.mjs:9` concedes this by design |
| **Rate limits** (300 create/min, 1200 read/min; 1,000-scheduled-deployment cap; 10s jitter) | Out of scope — org/infra-level policy, not modeled in the core wire | no per-org rate-limit middleware in `protocol-managed` |
| **100k tool-output / oversized-block spill to file (preview + path)** | Partial / deferred — tracked open item **C11**; compaction covers token/message windows, not single-block spill | 6 compaction suites are window-only |

## Redundancy / dead-code assessment

The goal's cleanup clause ("is the uncovered code redundant/duplicate — if so, clean up"):
**no redundant or dead code was found** in the e2e-uncovered surface.

- Code uncovered by the HTTP e2e but **covered by Rust unit/integration tests** is
  deliberately excluded from the e2e-surface figure — see the `coverage.sh` header: runtime
  extensions (`memory/compact/mcp/tool-pattern`), the ACP/sandbox execution substrate, and
  the multi-backend content-addressed store (`awaken-file-store`: e2e drives only its in-mem
  backend; Fs/Pg/S3 have their own Rust tests). This is separation of test tiers, not
  redundancy.
- The uncovered *doc-feature* code is the ▲ table above — absent features, not dead code.
- Structural duplication that *did* exist (durable store backends inlined in the
  `awaken-runtime-host` god-hub) was removed by the Step-3b re-layout: `awaken-env-store`,
  `awaken-work-store`, `awaken-session-store`, `awaken-session-contract`, and
  `awaken-managed-routers` are now their own leaves.

## Coverage tracking

- **Structural conformance**: `npm run test:conformance` — event catalog + `MANAGED_BETA` +
  serde golden vs the installed SDK. Green.
- **Behavioral coverage**: the ~170 `*_e2e.mjs` suites, +3 this pass (104 in the default
  `test` script; the management surface in `test:extended`).
- **Rust line/region coverage attributable to the e2e**: `bash e2e/coverage.sh` (instruments
  `awaken-server`/`awaken` via `cargo llvm-cov`, drives the full suite, reports *overall* and
  *e2e-surface* figures). Run on demand — it does a dedicated instrumented build.
