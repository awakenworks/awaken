# Managed Agents — Docs-Driven e2e Coverage

Test-coverage design and gap analysis for the **Claude Managed Agents** API
(`platform.claude.com/docs/en/managed-agents/*`), mapped against awaken's TS/Node
e2e conformance suite in `e2e/` (246 `*_e2e.mjs` / `*_e2e.ts` files + the static conformance
gate in `e2e/conformance/`).

The oracle is the installed official SDK (`@anthropic-ai/sdk`, pinned in
`e2e/package.json`); the subject under test is awaken's Rust wire vocabulary. The
static gate (`npm run test:conformance`) already pins awaken's event catalog +
`MANAGED_BETA` + the serde golden to that SDK — the audit below is about *behavioral*
coverage on top of that structural conformance.

## Exhaustive contract baseline (2026-07-29)

The pinned English sitemap contains 28 Managed Agents pages. This document is the
single docs-driven coverage owner for all of them. `PROTOCOL_COMPATIBILITY_TEST_DESIGN.md`
continues to own only the cross-protocol normalization method; tests must not duplicate
the Managed state machine there or create one suite per documentation page.

Files were excluded from the implementation initiative that originally added this
baseline; their existing behavior remains visible in the traceability table. Dreams
are implemented as a research-preview surface. Rust owns the detailed lifecycle,
JSONL, transient-File retention, and mount alternatives; one official TypeScript SDK E2E owns only the
cross-module contract, so it does not duplicate the state-machine suite.

### Page-by-page traceability (official sitemap snapshot: 2026-07-30)

This is the canonical page inventory. The offline docs gate checks that these exact
28 official slugs occur once, every row names executable evidence, and every row
records a test-design method. `✅` means Awaken-owned behavior is executable locally;
`◇` means the page also contains Anthropic-hosted infrastructure, Console UX, SDK
client logic, branding, or operator responsibilities that Awaken cannot truthfully
execute, while the local API/security boundary is still covered.

| Official page | Contract and scope | Executable evidence | Test design / status |
|---|---|---|---|
| [agent-setup](https://platform.claude.com/docs/en/managed-agents/agent-setup) | Agent defaults; model/effort; replace/clear/metadata merge; optimistic version; no-op; versions; frozen rosters; archive terminality | `management_agents_e2e.mjs`, `agent_publication_truth_e2e.ts`, `managed_agent_model_inherit_e2e.mjs` | Decision B; state transition + CAS + null/omit/empty partitions ✅ |
| [budgets](https://platform.claude.com/docs/en/managed-agents/budgets) | Session/Deployment hard list-cost ceiling; immutable exact price snapshot; cumulative usage; one-way removal; threshold admission and budget-reached lifecycle | `management_sessions_family_e2e.mjs`, protocol `budget.rs`, Session application `budget.rs` | Exact integer cause/effect + threshold/replay/update/removal decision table ✅ |
| [cloud-sandboxes-reference](https://platform.claude.com/docs/en/managed-agents/cloud-sandboxes-reference) | Cloud configuration and fail-closed provisioning; Anthropic's exact Ubuntu image, CPU/RAM/disk inventory is external infrastructure | `management_environments_e2e.mjs`, `managed_container_agent_e2e.mjs`, `sandbox_provisioning_e2e.mjs` | Config equivalence + real provider success/failure; hosted inventory explicitly non-applicable ◇ |
| [define-outcomes](https://platform.claude.com/docs/en/managed-agents/define-outcomes) | Rubric admission; one active outcome; iterations/spans; satisfied/revision/max/failed/interrupted terminals; output scope | `managed_outcome_e2e.mjs`, `managed_outcome_lifecycle_e2e.mjs`, `managed_outcome_runtime_matrix_e2e.ts` | State-machine transition/pairing + boundary 1..20; G1-G4/H8 ✅ |
| [dreams](https://platform.claude.com/docs/en/managed-agents/dreams) | Create/retrieve/list/cancel/archive; frozen JSONL with bounded retention; read-only inputs/new writable output; usage/errors/limits; mid-run input invalidation; automatic policy uses same job path | `managed_dream_e2e.ts`, protocol `tests/dreams.rs`, server `managed_dream_e2e.rs` | Dream lifecycle + failure injection + cleanup/restart + official SDK ✅ |
| [environments](https://platform.claude.com/docs/en/managed-agents/environments) | Cloud/self-hosted config; packages; limited networking defaults/hosts/MCP/package managers; update/archive/delete; no filesystem sharing | `management_environments_e2e.mjs`, `managed_container_agent_e2e.mjs`, `managed_package_registry_e2e.mjs`, protocol `tests/environments.rs` | Decision F; config partitions + pre-agent package image + durable single-flight + authenticated OCI digest reuse after local-cache removal + provider fail-closed + lifecycle ✅; no-bypass runtime allowlist still needs a deployment egress gateway ◇ |
| [events-and-streaming](https://platform.claude.com/docs/en/managed-agents/events-and-streaming) | Send/list/stream; ordering/processed_at; preview admission and reconciliation; reconnect/dedupe; interrupt/redirect; thread/custom/HITL/system/usage events | `managed_reconnect_e2e.mjs`, `managed_real_thinking_e2e.mjs`, `managed_processed_at_e2e.mjs`, `managed_system_message_e2e.mjs` | Decisions E/H; sequence/state transition + 100/101 boundaries + reconnect fault injection ✅ |
| [files](https://platform.claude.com/docs/en/managed-agents/files) | Upload/download; create-time/live mounts; path defaults/parents; read-only inputs; copy identity; resource lifecycle/authorization | `managed_resources_api_e2e.mjs`, `management_files_models_e2e.mjs`, `managed_resource_mount_e2e.mjs`, `managed_resource_api_edge_e2e.mjs` | Union/path/boundary + authz + sandbox observation; Anthropic-only upload metadata noted ◇ |
| [github](https://platform.claude.com/docs/en/managed-agents/github) | Token non-disclosure; repository mount/checkout; running-session credential rotation without repository replacement; clone/MCP use | `managed_git_repo_e2e.mjs`, `managed_resource_lifecycle_e2e.mjs`, `secret_nonleak_e2e.mjs`, `managed_full_chain_e2e.mjs` | Secret-flow + lifecycle mutation + real clone partitions ✅ |
| [mcp-connector](https://platform.claude.com/docs/en/managed-agents/mcp-connector) | URL/name/count/reference validation; normalization; tool filters/policy; vault auth; typed retry/error; reconnect; oversized spill | `managed_mcp_e2e.ts`, `management_mcp_e2e.mjs`, `management_agents_e2e.mjs`, `managed_tool_result_e2e.mjs` | URL/cardinality/reference partitions + D2-D5 + failure taxonomy ✅ |
| [mcp-tunnels](https://platform.claude.com/docs/en/managed-agents/mcp-tunnels) | Research-preview Tunnel/Certificate wire belongs to Awaken; domain/token/certificate/transport custody and lifecycle are Cloud-only | `management_tunnels_contract_e2e.mjs`, protocol `tunnels.rs`, Cloud Tunnel tests | SDK method/path/header contract locally; aggregate, secret and failure decision tables on Cloud Full target ◇ |
| [memory](https://platform.claude.com/docs/en/managed-agents/memory) | Exclusive beta; 100KiB/2000 heads; max 8 attachments/4096-char guidance; RO/RW; segment prefix/depth/basic/full; CAS/history/redaction/lifecycle | `managed_memory_repository_durable_e2e.mjs`, `management_memory_stores_e2e.mjs`, `managed_memory_extraction_durable_e2e.mjs`, protocol `session_resources.rs` | ML1-ML7/MC1-MC4/M1-M4; boundary + CAS + restart + mount denial ✅ |
| [migration](https://platform.claude.com/docs/en/managed-agents/migration) | Messages/Agent-SDK mapping to durable Agent/Environment/Session/events; built-in vs client tools; idle; pinning; resources/vaults/policies | `managed_full_lifecycle_e2e.mjs`, `managed_custom_e2e.mjs`, `management_agents_e2e.mjs`, `management_vaults_family_e2e.mjs` | End-to-end mapping + differential official-SDK DTO check; client-only planning/max-turns excluded ◇ |
| [multiagent-orchestration](https://platform.claude.com/docs/en/managed-agents/multiagent-orchestration) | Shared sandbox/vault but isolated persistent threads; agent-scoped config; self; frozen version roster; one level; max 20; Advisor reserved member and consultation lifecycle; follow-up/archive/interrupt | `managed_delegation_e2e.mjs`, `management_agents_e2e.mjs`, protocol thread-selector tests | Roster 1/20/21 + advisor 0/1/2, reserved-name/model/geo pairing + consultation failure/redaction/interrupt rules; thread state transitions ✅ |
| [onboarding](https://platform.claude.com/docs/en/managed-agents/onboarding) | Console prototype/copy-code flow and inline runner map to the same Agent/Environment/Session/events resources | `managed_full_lifecycle_e2e.mjs`, `management_agents_e2e.mjs`, `management_environments_e2e.mjs` | API journey E2E; Anthropic Console rendering/copy UX is external ◇ |
| [overview](https://platform.claude.com/docs/en/managed-agents/overview) | Core resource graph, built-in capabilities, state/history, steering/interrupt, scheduled execution and endpoint beta gate | `managed_full_lifecycle_e2e.mjs`, `managed_capabilities_e2e.mjs`, `managed_contract_guard_e2e.mjs`, `management_deployments_e2e.mjs` | Critical-path journey + decision A + lifecycle smoke ✅ |
| [permission-policies](https://platform.claude.com/docs/en/managed-agents/permission-policies) | Agent default allow/MCP default ask; enable/disable; frozen sessions; all blocker confirmations; allow/deny; custom-tool exclusion | `management_mcp_e2e.mjs`, `managed_hitl_e2e.mjs`, `managed_tool_result_e2e.mjs` | Decision D; pending-set sequence + approve/deny/no-result partitions ✅ |
| [quickstart](https://platform.claude.com/docs/en/managed-agents/quickstart) | Official SDK create Agent/Environment/Session, open stream before send, tool execution, idle; Memory header exception | `managed_full_lifecycle_e2e.mjs`, `managed_e2e.mjs`, `management_agents_e2e.mjs`, `managed_contract_guard_e2e.mjs` | Executable official-SDK happy path + ordering and header negatives ✅ |
| [reference](https://platform.claude.com/docs/en/managed-agents/reference) | Complete inbound/agent/session/thread/span/system/delta catalogs; worker flags contract; MCP transport; 300/1200 org limits | `gate_event_catalog_e2e.mjs`, `management_official_worker_e2e.mjs`, `mcp_official_sdk_e2e.ts`, protocol `rate_limit.rs` tests | Catalog golden + boundary/rate-window + official worker/SDK; branding is non-executable ◇ |
| [scheduled-deployments](https://platform.claude.com/docs/en/managed-agents/scheduled-deployments) | Cron/IANA/DST projection; five previews; jitter/capacity; pause/unpause/archive; manual/scheduled runs; error XOR/auto-pause; durable claim | `management_deployment_schedule_e2e.mjs`, `management_deployments_e2e.mjs`, server `managed_deployment_e2e.rs` | Decision G14-G17/S1-S7 + clock/state/failure/concurrency partitions ✅ |
| [self-hosted-sandboxes](https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes) | Queue/worker lifecycle; poll timing/reclaim; ack/heartbeat/stop; stats/liveness; queued-without-worker; resource limits; custom tools | `management_self_hosted_worker_e2e.mjs`, `management_official_worker_e2e.mjs`, `management_official_worker_run_e2e.mjs`, protocol `tests/environments.rs` | Poll P1-P5 + lease state/CAS/reclaim + official helper E2E ✅ |
| [self-hosted-sandboxes-security](https://platform.claude.com/docs/en/managed-agents/self-hosted-sandboxes-security) | Credential/workspace/network/filesystem trust boundaries and fail-closed worker behavior; image hardening/retention remain operator-owned | `secret_nonleak_e2e.mjs`, `management_authz_routes_e2e.mjs`, `sandbox_provisioning_e2e.mjs`, `worker_resource_manifest_e2e.ts` | Threat-boundary negative tests + holder/egress/path partitions; operator controls non-applicable ◇ |
| [session-operations](https://platform.claude.com/docs/en/managed-agents/session-operations) | Idle-only tools/MCP replace; retrieve/list/filter; bidirectional opaque order-bound pagination; archive/delete state gates and cascade scope | `management_sessions_family_e2e.mjs`, `managed_session_pagination_e2e.mjs`, `managed_session_deleted_e2e.mjs` | List L1-L7 + update state table + archive/delete transition/side-effect checks ✅ |
| [sessions](https://platform.claude.com/docs/en/managed-agents/sessions) | Agent/env/version binding; 1..50 atomic initial events; override omit/null/replace; body/file/outcome/resource/vault limits; lazy idle | `management_sessions_family_e2e.mjs`, `managed_model_override_e2e.mjs`, `acp_e2e.mjs`, `managed_resource_api_edge_e2e.mjs` | Decision C + boundary 0/1/50/51 + invalid-batch atomicity ✅ |
| [skills](https://platform.claude.com/docs/en/managed-agents/skills) | Skills beta; bundle/version/latest pin; uniqueness; max 20 attachments per Agent / max 500 across a composed Session; attach/freeze; filesystem executable/progressive disclosure | `management_agents_e2e.mjs`, `management_skills_e2e.mjs`, `managed_skill_bundle_pin_e2e.mjs`, `managed_skills_e2e.mjs`, `managed_contract_guard_e2e.mjs` | Header/count/version/bundle partitions (including 20/21 Agent boundary) + restart/pin observation ✅ |
| [tools](https://platform.claude.com/docs/en/managed-agents/tools) | Eight built-ins including the canonical WebSearch execution path; defaults/config/disable; custom declaration-use-result; client execution; 100000-char spill with preview/path | `managed_capabilities_e2e.mjs`, `managed_custom_e2e.mjs`, `managed_tool_result_e2e.mjs`, protocol `tool_output_spill` tests | Decision D + eight-member exact-set + 100000/100001 boundary + structured result loop ✅ |
| [vaults](https://platform.claude.com/docs/en/managed-agents/vaults) | Write-only secrets; MCP/static/env kinds; max 20; immutable structure/rotation; first match; all threads; archive/delete cascade; validation | `management_vaults_family_e2e.mjs`, `management_vaults_edge_e2e.mjs`, `secret_nonleak_e2e.mjs` | Kind/cardinality/validation partitions + lifecycle/rotation/secret-flow ✅ |
| [webhooks](https://platform.claude.com/docs/en/managed-agents/webhooks) | Supported event families; ID-only payload; HTTPS/public/secret/signature; dedupe; no order/backfill; retry/3xx/auto-disable; scheduled-run-only facts | `awaken-webhook/tests/webhook_e2e.rs`, `awaken-webhook-managed/tests/crud.rs`, `awaken-coordinator/tests/webhook_plane_e2e.rs`, protocol Deployment tests | G9-G13/W1-W2; response-class + SSRF/signature + retry + manual/scheduled differential ✅ |

### Complete cause and effect inventory

Every concrete test added under this initiative must carry a comment naming one rule
from the decision tables below. The comment is the durable test-design record; a second
case catalog must not be introduced.

Causes:

1. endpoint family and endpoint-specific beta-header set;
2. caller identity, Workspace ownership, resource existence and lifecycle state;
3. request schema, required fields, discriminators, counts, byte sizes and references;
4. effective Agent version and create-time override composition;
5. current Session/thread/deployment/environment status;
6. tool kind, enabled state, permission policy and client decision/result;
7. model, MCP, credential, network, package, Worker and sandbox availability;
8. revision/CAS/idempotency key, concurrent mutation, retry and process-failure point;
9. stream opt-in, connection timing, reconnect point and thread scope;
10. cron instant/timezone/DST, webhook response and asynchronous job state;
11. access mode, secret-injection location, retention and deletion/archival state.

Effects:

1. HTTP/SSE status and official DTO/error union;
2. atomic rejection with no row, event, work item, sandbox or external side effect;
3. version, metadata, configuration, credential and resource mutation;
4. Session/thread/deployment/job state transition and stop reason;
5. event type, identity, ordering, `processed_at`, usage and durable history;
6. tool/network/package/repository/memory side effect or its denial;
7. retry/reschedule/reclaim/auto-disable behavior;
8. list/stream/reconnect completeness and deduplication;
9. secret non-disclosure, egress scoping and Workspace isolation;
10. idle/terminated/completed/failed/canceled terminal outcome.

### Decision table A — request admission and endpoint headers

| Rule | Endpoint | Required beta set | Other condition | Effect |
|---|---|---|---|---|
| A1 | ordinary Managed endpoint | `managed-agents-2026-04-01` | valid request | continue to domain validation |
| A2 | ordinary Managed endpoint | missing Managed beta | any | reject before mutation |
| A3 | memory-store endpoint | `agent-memory-2026-07-22` only | valid request | continue to memory validation |
| A4 | memory-store endpoint | Managed + memory beta | any | 400, no mutation |
| A5 | non-memory Managed endpoint | memory beta only | any | reject before mutation |
| A6 | any endpoint | correct beta | malformed/oversized body | 400/413, no mutation |
| A7 | Skills endpoint | `skills-2025-10-02` | missing or Managed beta only | continue / reject before mutation |

### Decision table B — Agent update

| Rule | Archived | Version supplied | Matches current | Effective change | Effect |
|---|---:|---:|---:|---:|---|
| B1 | yes | any | any | any | reject; no version/event |
| B2 | no | yes | no | any | 409, including a semantic no-op |
| B3 | no | yes | yes | no | return current version; no update webhook |
| B4 | no | yes | yes | yes | increment once; publish update |
| B5 | no | no | n/a | no | return current version |
| B6 | no | no | n/a | yes | last-write-wins; increment once |

Scalar fields replace; list fields replace in full and clear on `null`/`[]`;
metadata merges by key and deletes a key on `null`; `multiagent` replaces as a
whole. An unchanged model id preserves omitted effort, while a changed model id
resets omitted effort to that model's default.

### Decision table C — Session creation and initial events

| Rule | `initial_events` | Effective overrides/limits | Effect |
|---|---|---|---|
| C1 | omitted or empty | valid | create idle; no execution |
| C2 | 1..=50 message/outcome events | valid | validate/persist in order; create running |
| C3 | contains any other event type | any | reject whole create; no Session |
| C4 | any event invalid | any | reject whole create; no partial events/Session |
| C5 | >50 | any | 400 |
| C6 | >1 outcome or missing rubric | any | 400 |
| C7 | valid | `model:null` | 400 `agent_model_required` |
| C8 | valid | tools cleared while skills remain | 400 |
| C9 | valid | MCP servers cleared while a toolset dangles | 400 |
| C10 | valid | list override supplied | replace in full, never merge |

### Decision table D — tool execution

| Rule | Tool kind | Enabled | Policy | Client response | Effect |
|---|---|---:|---|---|---|
| D1 | built-in/MCP | no | any | n/a | tool unavailable; no use event |
| D2 | built-in/MCP | yes | `always_allow` | n/a | execute automatically |
| D3 | built-in/MCP | yes | `always_ask` | missing | idle/requires_action indefinitely |
| D4 | built-in/MCP | yes | `always_ask` | allow all blockers | execute and resume |
| D5 | built-in/MCP | yes | `always_ask` | deny | do not execute; rejected result reaches model |
| D6 | custom | yes | ignored | missing | idle/requires_action |
| D7 | custom | yes | ignored | success/error result | route to originating thread and resume |

### Decision table E — live preview and reconnect

| Rule | Opt-in | Event/connection | Effect |
|---|---|---|---|
| E1 | none | message | buffered event only |
| E2 | `agent.message` | online | start, zero-or-more deltas, authoritative buffered event; ids agree |
| E3 | `agent.thinking` | online | start only; no delta |
| E4 | invalid value or >100 values | any | 400 |
| E5 | valid | server sheds deltas | preview is a contiguous prefix; buffered event remains complete |
| E6 | valid | disconnect/reconnect | deltas never replay; list history supplies buffered events |
| E7 | valid | child activity on primary stream | no child preview; use the child stream |

### Decision table F — environment and egress credential

| Rule | Environment reaches host | Credential allows host | Injection location enabled | Client sends placeholder verbatim | Effect |
|---|---:|---:|---:|---:|---|
| F1 | no | any | any | any | network request denied |
| F2 | yes | no | any | yes | request may leave, literal placeholder remains |
| F3 | yes | yes | no | yes | literal placeholder remains in disabled location |
| F4 | yes | yes | yes | yes | substitute only at egress; sandbox never sees secret |
| F5 | yes | yes | yes | no | local validation/signature fails; no hidden fallback |
| F6 | self-hosted | n/a | environment-variable credential | any | reject unsupported binding |

### Decision table G — outcomes, deployment and webhook

| Rule | Cause | Effect |
|---|---|---|
| G1 | outcome satisfied | idle |
| G2 | outcome needs revision | next contiguous iteration |
| G3 | max iterations | final acknowledgment, then idle |
| G4 | outcome failed/interrupted | terminal evaluation, then idle |
| G5 | scheduled transient/rate-limit failure | failed run; deployment remains active |
| G6 | archived environment/vault/subagent | failed run; deployment auto-pauses with same error |
| G7 | primary Agent archived/deleted | deployment archives; no run row |
| G8 | manual run while paused | run is allowed |
| G9 | webhook 2xx | acknowledge and reset failure window |
| G10 | webhook 3xx | never follow/retry; disable immediately |
| G11 | webhook 4xx/5xx/transport failure | at most three jittered-backoff attempts; drop after final failure |
| G12 | endpoint resolves non-public | disable immediately |
| G13 | duplicate/late/out-of-order webhook | stable event id; consumer dedupes and fetches current resource |
| G14 | scheduled deployment count reaches 1,000 in one organization | reject the next scheduled create/update atomically; unscheduled and archived rows do not consume capacity |
| G15 | exact cron occurrence reaches its bounded jitter due time | one run; `scheduled_at` and previews retain the exact unjittered occurrence |
| G16 | deployment is unpaused after missed occurrences | resume from the next future occurrence; never backfill missed runs |
| G17 | deployment is archived | archive is idempotent and terminal; update/pause/unpause/manual run reject |
| G18 | DeploymentRun trigger is manual / scheduled | both persist; only scheduled runs emit `deployment_run.*` webhook facts |

### Decision table H — event-batch admission and processing

| Rule | Cause | Constraint/state | Effect |
|---|---|---|---|
| H1 | `system.message` has 1 text item | supported primary model; no pending tool | accept and persist |
| H2 | `system.message` has 1000 text items | same as H1 | accept inclusive maximum |
| H3 | `system.message` has 0 or 1001 items | any | 400; reject whole batch before persistence |
| H4 | `system.message` has valid content | unsupported primary Claude model | 400 `model_does_not_support_mid_conversation_system`; no event |
| H5 | system alone or with user message | idle `requires_action` | 400; pending tool and history unchanged |
| H6 | matching tool result/confirmation precedes system | idle `requires_action` | resume, then accept system in request order |
| H7 | result id or built-in/custom kind mismatches pending tool | idle `requires_action` | 400; no result/system event and pending remains resumable |
| H8 | outcome `max_iterations` is 1..=20 / outside range | valid outcome body | accept inclusive bounds / 400 without persistence |
| H9 | accepted inbound event | queued / result-or-outcome immediate class | receipt id equals durable id; durable `processed_at` populated, immediate receipt populated only for documented classes |

### Required test-comment form

```rust
/// Causes: <inputs, state, dependency and failure trigger>.
/// Constraints: <invalid combinations or ordering rule>.
/// Effects: <response, state, events, side effects and terminal outcome>.
/// Decision rule: <table>/<rule>.
```

## Method

Every documented page was decomposed into concrete testable behaviors, then
cross-referenced against the actual suites. Test-design methods applied per surface:
equivalence partitioning (network policies, package managers, rubric kinds), boundary
values (page `limit`, compaction window, `max_iterations`, 100k spill), state-transition
coverage (session status, deployment lifecycle, multiagent thread lifecycle, outcome
result machine), and error-guessing (fabricated cursors, unparseable cron, unreachable
MCP servers).

Each row is classed by **executable ownership**, because an Anthropic-hosted behavior is
not a local coverage hole in a self-hostable conformant core.

- **✅ Locally executable** — every Awaken-owned behavior named by the row has direct
  executable evidence.
- **◇ Boundary-covered external scope** — the page also describes Anthropic-hosted
  infrastructure, Console/SDK UX, branding, or operator controls; the local API and
  security boundary is executable, but no false local execution claim is made.

Older historical sections below retain `▲` only when quoting the state of an earlier
audit; it is not a current matrix status.

## Cause-effect completeness and Ark portability (2026-07-24)

The compatibility oracle for the session-update and create-override cases is Anthropic's
official Managed Agents contract, not Ark. In particular, Anthropic specifies that
`tools`/`mcp_servers` updates are full replacement, that only those two fields are
mutable mid-session, and that create-time list overrides can clear a session-local field.
See the [official session operations contract](https://platform.claude.com/docs/en/managed-agents/session-operations),
[Update Session API reference](https://platform.claude.com/docs/en/api/beta/sessions/update),
and [MCP connector constraints](https://platform.claude.com/docs/en/managed-agents/mcp-connector).

The current inventory contains 246 `*_e2e.mjs` / `*_e2e.ts` files, 136 of them
managed-named. The 26-page inventory is complete for the sitemap snapshot: locally owned
contracts have executable evidence, while hosted/cloud/operator-only effects are marked
`◇` and are not counted as locally executed.

The cause-effect graph has four materially different cause classes. A remote Ark endpoint
can exercise only the provider-controlled part of the graph; it cannot replace the local
host, deployment, persistence, fault-injection, and security fixtures.

| Cause class | Representative causes | Required observable effects | Current coverage | Can Ark be the sole fixture? |
|---|---|---|---|---|
| Public wire contract | route prefix, beta header, DTO discriminator, pagination, SSE framing | official SDK sends/parses the exchange without a compatibility fork | strong static gates; behavioral mismatches remain provider-dependent | **Partial** |
| Managed session state machine | create/update, message, tool loop, confirmation, outcome, threads, interrupt, archive/delete | legal status/event sequence and terminal state | happy paths strong; conditional branches include the backlog below | **Conditional** on provider feature support |
| Host/runtime resilience | restart, reconnect/replay, lease reclaim, exactly-once, filesystem/sandbox enforcement | durable recovery and invariant preservation | local process/database/fake-upstream suites | **No** |
| Deployment/security/operations | worker topology, IAM/authz, networking, telemetry, secret handling, cross-protocol ingress | policy enforcement, isolation, metrics/traces, failure taxonomy | local/container/infra fixtures | **No** |

The dynamic compatibility path is:

`route/auth → create/retrieve → user.message → running → model/tool → optional
requires_action/resume → idle/outcome → list/stream/reconnect → archive/delete`.

The TypeScript recorder `conformance/ark_managed_agents_compat_e2e.ts` drives that path
with the official SDK and stores a redacted exchange transcript under
`artifacts/ark-managed-agents-compat.json`. Against Ark on 2026-07-24 using
`@anthropic-ai/sdk` 0.114.0, the result was **14 pass / 18 fail / 3 skip**. The principal
causes were the SDK `/v1` suffix versus Ark's `/api/v3` route, missing required response
members and nullable `next_page`, unnamed `data:`-only SSE frames, unsupported archive
operations, and accepted-but-not-observed custom-tool / `always_ask` updates. Therefore
Ark is useful as one provider compatibility lane, but it cannot be the canonical or only
Managed Agents e2e environment.

This pass closed the following local contract cases against that Anthropic oracle:

| Case | Local coverage | Verification |
|---|---|---|
| Session `agent.tools` full replacement | implemented and asserted | `management_sessions_family_e2e.mjs` |
| Session `agent.mcp_servers` full replacement | implemented and asserted through the one durable attachment lifecycle | `managed_mcp_hot_swap_e2e.ts` calls only the newly active Native generation across add/replace/remove; `managed_mcp_recovery_e2e.ts` proves restart; `acp_managed_mcp_e2e.mjs` proves a relaunched ACP receives only the replacement generation and then the empty set after removal |
| Create-time null/empty overrides | `system`, `tools`, `mcp_servers`, `skills`; `model:null` remains 400 | `managed_model_override_e2e.mjs` |
| MCP server ↔ `mcp_toolset` references | dangling server rejected; declared toolset projects | `managed_model_override_e2e.mjs` |
| MCP `always_ask` confirmation | permission policy parks and resumes MCP calls | `management_mcp_e2e.mjs` |
| Retryable inference and active interrupt/steer | `status_rescheduled` is emitted before retry; active turn can be interrupted and replaced | `managed_error_recovery_e2e.mjs` |

The DeepSeek live-provider lane was also attempted from the shell's
`DEEPSEEK_API_KEY` configuration with the OpenAI-compatible endpoint and
`deepseek-v4-flash`. Transport and model resolution succeeded, but the provider returned
HTTP 402 (`Insufficient Balance`), so no model-output compatibility verdict is claimed
from that run. Deterministic local compatibility remains green: Rust/SDK event catalogs
27/27 outbound and 7/7 inbound, beta sentinel, serde golden, TypeScript type-check, and
the session override/update suites all pass.

The ACP runtime-selection regression is also closed: a session's
`metadata["awaken.runtime"] = "acp:<cli>"` is copied into the neutral resolved
backend binding before the shared `AttemptExecutorRegistry` is built. This prevents
an accepted runtime hint from silently falling back to the native model.
`acp_e2e.mjs` and `acp_jsonrpc_e2e.mjs` now pass, including native-session
non-regression and second-turn relaunch coverage.

## Closed this pass

| Behavior | Doc page | New suite | Design |
|---|---|---|---|
| Session-list filtering and bidirectional opaque pagination (`?limit=&page=&order=`, `{data,next_page,prev_page}`) | session-operations | `managed_session_pagination_e2e.mjs` | L1-L7: default desc/asc, next/prev transitions, filter intersections, archived partition, fabricated/order-mismatched cursor → 400 |
| Outcome evaluation lifecycle spans (`span.outcome_evaluation_start`/`_end` bracket every iteration; stable `outcome_id`; monotonic `iteration`) | define-outcomes / reference | `managed_outcome_lifecycle_e2e.mjs` | state-transition: each `_start(outcome_id,iteration)` pairs with one `_end` at the same key; iterations contiguous ascending |
| Scheduled deployments (cron `schedule` echo/persist, pause retains, write-time cron validation) | scheduled-deployments | `management_deployment_schedule_e2e.mjs` | equivalence partition on cron expr (valid / garbage / missing → 400) + state-transition pause→unpause preserves schedule |
| Session `initial_events` executes through the canonical event executor (0/1/50/51, allowed kinds, at-most-one outcome) and returns `running` while queued | start-a-session / reference | existing adapter tests + `acp_e2e.mjs` | decision table C; native and ACP initial turns share the same executor |
| Inbound events persist under their receipt id and converge `processed_at` according to the documented immediate/queued classes | events-and-streaming / reference | existing adapter tests + `managed_system_message_e2e.mjs` | decision H9; no receipt-only shadow path |
| `system.message` content bounds, primary-model capability, and `requires_action` ordering | events-and-streaming | existing adapter/HITL tests + native/ACP SDK E2E | decision H1-H7; whole-batch rejection precedes mutation |
| Event-delta admission at 100/101 and start-only thinking reconciliation | events-and-streaming | existing streaming suite | decision E3/E4; thinking content never crosses the wire and committed ids equal preview ids |
| Managed beta gate covers ordinary Session/Agent/Environment/Deployment/Vault families; Skills uses its own beta | overview / skills / reference | existing contract-guard + Skills E2E | decision A1/A2/A5/A7; Memory follows the exclusive A3/A4 rule below |
| Memory Store endpoints require only `agent-memory-2026-07-22`; missing, Managed-only, or both headers reject before domain work | memory / beta-headers / reference | existing `managed_contract_guard_e2e.mjs` + Memory family/lifecycle suites | decision A3/A4; one prefix gate covers the collection and every subresource |
| Memory listing/capacity/attachment boundaries (segment prefix, depth 0/1, basic/full, 2,000 heads, 8 stores, 4,096 characters) | memory | `managed_memory_repository_durable_e2e.mjs` + repository/protocol tests | ML1-ML7/MC1-MC4/M1-M4; inclusive boundary, invalid partition, restart, API/mount shared owner |
| Self-hosted work long poll (`block_ms` omitted/null/1..999/outside) | self-hosted-sandboxes | protocol `tests/environments.rs` + official worker E2E | P1-P5; timing boundary + validation + no second queue |
| Manual vs scheduled DeploymentRun webhook emission | webhooks / scheduled-deployments | protocol Deployment repository test | G18/W1-W2; differential trigger partition + restart durability + outbox absence/presence |

Verified against source before writing: `deployments.rs` (`projected_schedule`/`active_cron` +
write-time `Cron::parse` 400), `cron.rs` (dependency-free 5-field evaluator),
the Session-list adapter (order-bound bidirectional cursor over the canonical list), `types/session.rs`
(`SpanOutcomeEvaluationStart{outcome_id,iteration}`). The behavior E2Es are registered in
`package.json`; the 26-page matrix is enforced by `test:docs-coverage`.

## Former implemented test gaps

All non-excluded behavior in this former backlog now has executable coverage.

1. ~~**MCP tool confirmation (`always_ask`) approve/deny**~~ — closed in `management_mcp_e2e.mjs`.
2. ~~**MCP `session.error` classification**~~ — closed in
   `management_mcp_e2e.mjs`: 401 and unreachable-server partitions now project through
   the neutral classified `RunError` into `session.error` with server name and retry status.
3. ~~**Mid-run interrupt + steer**~~ — closed in `managed_error_recovery_e2e.mjs`.
4. ~~**Session agent-update gate**~~ — closed in `management_sessions_family_e2e.mjs`.
5. ~~**Overrides clearing rules**~~ — closed in `managed_model_override_e2e.mjs`.
6. ~~**`agent.thinking` start-only preview + no-replay-on-reconnect**~~ — closed in
   `managed_real_thinking_e2e.mjs`: a thinking-capable live provider produces one
   start-only preview; the durable contentless marker reuses its id; the client drops
   that SSE connection, immediately reopens with the same opt-in, observes no replayed
   `event_start`/`event_delta`, and recovers the complete answer plus idle from the
   reopened stream and authoritative history. The deterministic Rust streaming suite
   independently covers private-content suppression and preview reconciliation.
7. ~~**Deployment run failure taxonomy + auto-pause**~~ — closed across
   `management_deployments_e2e.mjs` and `management_deployment_schedule_e2e.mjs`:
   the exact tagged run-error union replaces the former free-form/fixed-null field;
   `session_id`/`error` is a structural XOR; manual persistent and scheduled
   transient failures retain an active deployment, while a scheduled persistent
   failure appends the failed run and auto-pauses with the exact matching reason.
   Typed deployment/run list queries cover error, trigger, lifecycle, agent, RFC3339
   time, archive and pagination partitions. Cron matching now uses the declared IANA
   timezone and exposes five ordered future occurrences for active/paused schedules,
   clearing them on archive.
8. ~~**`limited` networking sub-flags**~~ — closed across
   `management_environments_e2e.mjs` and the Rust `session_egress` behavior suite:
   omitted/null defaults are false, update omission preserves exact aggregate state,
   MCP access expands only the Session's normalized declared targets, package-manager
   access expands the one canonical registry policy, and disabled/empty inputs add no
   ambient fallback host. The resulting policy is frozen into `SessionInit`; providers
   without no-bypass allowlist enforcement continue to reject it fail-closed.
9. ~~**Worker lease-reclaim**~~ — `management_environments_e2e.mjs` proves the
    `reclaim_older_than_ms=0` HTTP boundary and `awaken-work-store` proves the same
    requested age against SQLite durable state.
10. ~~**`read_only` memory mount rejects writes**~~ — closed in
    `managed_memory_extraction_durable_e2e.mjs` (activation fails closed, no extraction
    outbox, and no store mutation).
11. ~~**`mcp_toolset` declaration + tool filtering**~~ — declaration/reference validation
    is covered in `managed_model_override_e2e.mjs`; confirmation policy is covered in
    `management_mcp_e2e.mjs`.
12. ~~**Multiagent roster and thread control**~~ — delegation lifecycle, child
    enumeration/retrieval, fail-closed roster handling, and idle-child archive are
    covered by `managed_delegation_e2e.mjs`. Its Native coordinator exercises a
    Native member, an isolated frozen `{type:"self"}` copy, and a roster member whose
    frozen backend routes the child through the external ACP executor. The management
    API suite covers the 1..=20 cardinality, duplicate/self constraints, missing,
    archived, nested-coordinator and exact/current version-reference partitions. The
    same suite executes coordinators created before and after a worker update: the old
    coordinator runs the worker's exact v1 publication while the new coordinator runs
    v2, proving execution consumes the frozen revision rather than current catalog state.
    The protocol cause/decision-table test
    `interrupt_selector_targets_one_thread_or_all_non_terminal_threads` covers the
    documented optional `user.interrupt.session_thread_id`: a named
    `requires_action`/idle child targets only that child Run, omission targets the
    primary plus every non-terminal child, and unknown/terminal selectors fail
    before receipt persistence.
13. **Files negatives** — filename validation and download authorization are covered
    by `managed_resources_api_e2e.mjs`; `downloadable:false` upload metadata and
    `document`/`image` `file_id` blocks are absent from awaken's file-upload contract,
    so those Anthropic-cloud-only cases cannot be asserted locally.
14. ~~**`session.status_rescheduled` / `rescheduling`**~~ — closed in
    `managed_error_recovery_e2e.mjs` with a deterministic transient-503 scenario.

## Former gaps and explicit exclusions

This table records the audited closure state of documentation gaps. Implemented
rows name their executable evidence; exclusions explain why no implementation is
expected in this parity scope.

| Doc surface | Status in awaken | Evidence |
|---|---|---|
| **Dreams** (`/v1/dreams`, `dreaming-2026-04-21` header, create/poll/cancel/archive) | Implemented over one CAS lifecycle on the selected SQLite/Postgres Session store. Every process freezes the stable ordinary Dream Agent id; publishing that id through the normal Agent surface customizes it without a Dream-specific configuration API. Transient transcript Files are purged at terminal cleanup. | canonical design: [`managed-dream.md`](../docs/design/managed-dream.md); protocol tests: `awaken-protocol-managed/tests/dreams.rs`; Rust cross-module E2E: `awaken-coordinator/tests/managed_dream_e2e.rs`; official SDK E2E: `managed_dream_e2e.ts`; SDK declaration gate: `conformance/sdk_surface_coverage_e2e.mjs` |
| **Cloud env `packages` provisioning** (pip/npm/apt/cargo/gem/go, version pinning) | Implemented through the one neutral Sandbox provisioning seam. Docker and Podman resolve the selected base image to its exact local ID, build/reuse a content-addressed derived image, and the real workload observes the installed effect. Providers without package provisioning reject before workload creation; there is no fallback. | Admission/update semantics: `management_environments_e2e.mjs`; real success/fail-closed behavior: `managed_container_agent_e2e.mjs`; provider/cache side effects: `awaken-sandbox-container` cause-table tests |
| **Managed request rate limits** (300 Create/min, 1,200 Read/min, organization-scoped token buckets) | Implemented at the one merged Managed composition edge; flat and Workspace-addressed requests, Native Sessions, ACP Sessions, Dreams, and Dream policies share the appropriate organization buckets. Files, non-Managed routes, and non-Create mutations are not charged. | `rate_limit` cause/decision-table tests in `awaken-protocol-managed`; `management_surface::flat_and_workspace_paths_share_one_organization_create_bucket` |
| **Scheduled-deployment capacity and execution jitter** (1,000 scheduled deployments/organization; stable jitter bounded by 15% of the interval, minimum 5 seconds, maximum 9 minutes) | Implemented with durable SQLite/Postgres Deployment/DeploymentRun records and atomic `(Deployment, scheduled_at)` claims. Capacity is atomic across create/update/archive; previews and trigger contexts retain exact cron instants; execution uses the official stable interval-derived bound. Unpause skips missed occurrences, archive is terminal, internal Session creation shares the organization Create bucket, and primary-Agent archive cascades without a run. | canonical design: [`managed-deployments.md`](../docs/design/managed-deployments.md); `routes::deployments::tests`; `managed_deployment_e2e.rs`; `management_deployment_schedule_e2e.mjs` |
| **Automatic Dream policy** | Awaken extension, opt-in per `(Workspace, MemoryStore)`, default disabled; exact-version policy advance and Dream insert commit atomically, so multiple scheduler replicas submit one canonical job through the shared Managed periodic driver. | `automatic_policy_is_opt_in_thresholded_and_reuses_the_dream_job_path`; `concurrent_policy_ticks_claim_one_dream_across_replicas`; `dream_policy_api_projects_defaults_validates_and_survives_restart`; `managed_dream_e2e.rs` |
| **100k tool-output / oversized-block spill to file (preview + path)** | Implemented once at the Session sandbox boundary for Native local/MCP/remote-hand/delegated/recovered/client results and external ACP result projections. `<=100,000` characters remain inline; larger content is stored whole at a stable jailed path and only a bounded preview + readable path is committed. Storage failure is fail-closed. This internal result file is not the excluded Files API. | `tool_output_spill` cause/decision-table test; Native `tools`/`awaiting`/`formal_refinement` rules; ACP executor cause table; `acp_jsonrpc_e2e.mjs` reads and verifies complete files produced by both Native and ACP runs |

## Redundancy / dead-code assessment

The goal's cleanup clause ("is the uncovered code redundant/duplicate — if so, clean up"):
**no redundant or dead code was found** in the e2e-uncovered surface.

- Code uncovered by the HTTP e2e but **covered by Rust unit/integration tests** is
  deliberately excluded from the e2e-surface figure — see the `coverage.sh` header: runtime
  extensions (`memory/compact/mcp/tool-pattern`), the ACP/sandbox execution substrate, and
  the multi-backend content-addressed store (`awaken-file-store`: e2e drives only its in-mem
  backend; Fs/Pg/S3 have their own Rust tests). This is separation of test tiers, not
  redundancy.
- Remaining exclusions are the explicitly identified Anthropic-cloud-only Files
  behavior above; the implemented rows in the table carry direct test evidence.
- Structural duplication that *did* exist (durable store backends inlined in the
  `awaken-runtime-host` god-hub) was removed by the Step-3b re-layout: `awaken-env-store`,
  `awaken-work-store`, `awaken-session-store`, and `awaken-session-contract` are
  now their own leaves. Managed resource routes are consolidated into the single
  `awaken-protocol-managed` wire owner.

## Real-LLM validation (live KIMI)

The `real` server mode backs the managed session with a live model (`GenaiExecutor`,
`ANTHROPIC_API_KEY/BASE_URL/MODEL`). Validated against the KIMI Anthropic-dialect
endpoint (`https://api.kimi.com/coding/v1/`, `kimi-for-coding`):

| Doc behavior | Suite | Result |
|---|---|---|
| Session drives a real model return (`user.message` → `agent.message` → idle) | `managed_real_e2e` | ✅ |
| AI-SDK adapter over a real model | `ai_sdk_real_e2e` | ✅ |
| config → resolve → run with a real model | `managed_resolved_real_e2e` | ✅ |
| Live credential validation | `management_validate_e2e` | ✅ |
| Real file read + artifact write/retrieve + memory write-back cross-session | `managed_resources_e2e` | ✅ |
| **Built-in tool loop with a real model** (`agent.tool_use{bash}` → `requires_action` → `user.tool_confirmation` → sandbox exec → `agent.tool_result` → answer → `end_turn`) | `managed_real_tool_loop_e2e` (new) | ✅ |
| **Multi-turn context carryover** across a persistent session (real recall of turn-1 facts) | `managed_real_multiturn_e2e` (new) | ✅ |

Registered in `package.json` `test:real`. The `when-online` smoke is **not** applicable to
KIMI — it targets the real Anthropic **Managed Agents** API (`/v1/sessions`), which KIMI
does not expose (KIMI is a plain Anthropic-dialect `/messages` endpoint; the smoke 404s);
it needs a genuine Anthropic Managed key.

### ✅ Fixed: `agent.thinking` now surfaces from provider reasoning

Real-LLM testing disproved the prior deferral premise ("providers emit no reasoning"):
KIMI's raw `/messages` returns `thinking` blocks, and awaken now surfaces them as the
`agent.thinking` marker. Implemented end to end (commit `2d16905ce`):

1. `awaken-provider-genai`: enable genai's `capture_reasoning_content`; accumulate
   `ReasoningChunk` + `StreamEnd.captured_reasoning_content`; fold it into a leading
   `ContentBlock::Thinking` (reasoning is never replayed to the provider as input).
2. `awaken-agent-contract`: add `ContentBlock::Thinking` (ignored by `extract_text`) and a
   contentless `Fact::AssistantThinking`; the fold emits the marker when a `Thinking` block
   is present; `classify` treats it as message-tier truth.
3. `awaken-protocol-managed`: transcode `Fact::AssistantThinking` → `OutboundKind::AgentThinking {}`
   — the SDK's `BetaManagedAgentsAgentThinkingEvent` is `{id, processed_at, type}` with **no
   content**, so the reasoning text stays off the answer wire. AI-SDK / AG-UI drop the marker
   (not in their vocabulary).

Validated live: `managed_real_thinking_e2e` (KIMI) asserts the start-only preview,
contentless durable marker, disconnect/reconnect no-replay rule, answer recovery, and
that no reasoning text leaks. **No regression**: echo mode produces no reasoning →
no marker → the serde golden and all deterministic suites are unchanged. Blast radius was
small (base-enum additions rippled to only a handful of `_`-less matches). This moves
`agent.thinking` from ▲ to ✅.

## Coverage tracking

- **Structural conformance**: `npm run test:conformance` — event catalog + `MANAGED_BETA` +
  serde golden vs the installed SDK. Green.
- **Behavioral coverage**: 246 `*_e2e.mjs` / `*_e2e.ts` files, including 136 managed-named
  files (104 in the default `test` script; the management surface in `test:extended`).
  **Cumulative run of the default
  suite (each suite spawned independently): 104 pass / 0 fail / 104** — the whole deterministic
  suite is green. The 4 formerly-failing suites were fixed (they were real gaps, not
  environmental):
  - `managed_git_repo_e2e` + `managed_full_chain_e2e` — ADR-0038's `push_repo_at` pushes only
    what the agent *committed*, but the fake-upstream scripts wrote the repo file without a
    commit → nothing to push. Fixed: the scripted agent now runs a `bash` commit in the jail
    before harvest.
  - `durable_worker_metrics_e2e` + `brain_drain_e2e` — `awaken-scenario-host` never registered
    the `awaken_brain_active_streams` gauge (parity gap vs the real `awaken` binary), the
    `awaken_brain_draining` gauge was never implemented (drain state was `/readyz`-only), and
    the test parser didn't handle OTel's `{otel_scope_name=...}` labels. Fixed: register both
    gauges in the shared `register_active_streams_gauge`; tolerant Prometheus parsing.
- **Rust line/region coverage attributable to the e2e**: `bash e2e/coverage.sh` (instruments
  `awaken-coordinator`/`awaken` via `cargo llvm-cov`, drives the full suite, reports *overall* and
  *e2e-surface* figures). Run on demand — it does a dedicated instrumented build.
