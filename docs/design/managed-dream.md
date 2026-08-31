# Managed Dream

## End-to-end objective

Anthropic exposes this asynchronous resource as a **Dream**. Awaken names the
internal operation **Dream**: it freezes one source MemoryStore
and the committed histories of 1–100 Sessions, exports each transcript as JSONL,
and runs an ordinary auxiliary Agent against a new, independent MemoryStore.

```text
POST /v1/dreams
  -> durable DreamProcess
  -> atomic source-memory snapshot + independent output clone
  -> one Files JSONL artifact per selected Session
  -> ordinary Managed Session and Run
       /mnt/dream/input-memory                    read-only
       /mnt/dream/session-transcripts/*.jsonl     read-only
       /mnt/dream/output-memory                   read-write, write-through required
  -> completed | failed | canceled
  -> output MemoryStore remains independently addressable
```

Dream is not a new Agent state and is not context compaction. The auxiliary
Session retains the ordinary Session/Run lifecycle; `DreamProcess` owns only
cross-resource preparation, cancellation, cleanup, archive, and its Session
reference. Token usage is always projected from that ordinary Session.

Dream uses the ordinary, stable Agent id `awaken_builtin_dream_agent`. Publishing
that id through the normal Workspace Agent authoring flow customizes the full
executable snapshot; absence uses the built-in fallback. Automatic Dream policy
is opt-in and defaults off. When enabled for
one `(Workspace, MemoryStore)`, the production Managed scheduler evaluates it on
the same timer that drives cron Deployments and submits the same durable DreamProcess
as `POST /v1/dreams`; there is no second Dream executor or Agent state.

## Verified duplication review and change classification

The pre-implementation review found no existing Dream route, aggregate, or
executor. It did find authoritative implementations for every lower-level
responsibility. The implementation extends those owners rather than adding a
second Agent loop, transcript database, Memory truth, Files service, or sandbox
path.

### Reused unchanged

| Authority | Owner | Role in Dream |
|---|---|---|
| Managed Session/Event application | `awaken-protocol-managed::ManagedState` | validates Sessions, reads committed Messages, creates and archives the auxiliary Session |
| ordinary Agent execution | `awaken-runtime-host::SessionRuntime` | executes the Dream Agent through the normal Run path |
| Memory file truth | `awaken-resource-contract::MemoryRepository` | reads and commits path-addressed Memory content |
| Files data plane | `awaken-coordinator::SharedHost` | stores transient JSONL artifacts using the canonical File record and purge lifecycle |
| Resource catalog | `ResourceCatalog` | validates source ownership |
| Resources application | `MemoryStoreApplicationService` | is the sole output-store identity, lifecycle, retention, and purge command path |
| mount realization | provisioning and sandbox providers | realizes InlineBytes and MemoryStore inputs for the ordinary Session |

### Modified existing owners

| Owner | Change | Invariant gained |
|---|---|---|
| `MemoryRepository` and its volatile/SQLite/Postgres implementations | `snapshot_heads` | one backend-atomic, path-ordered source view |
| provisioning vocabulary | `MemoryWriteConsistency::{ProviderDefault, WriteThroughRequired}` | callers can require live Memory writes without naming FUSE |
| local, namespace, and container providers | reject copy realization for `WriteThroughRequired` | no silent copy/harvest downgrade before Agent launch |
| Managed beta middleware and rate-limit classifier | Dreams family and both beta capabilities | wire compatibility and existing Managed request governance |
| Managed Session application | exact built-in-origin exception and public realization seam | the built-in auxiliary Agent still uses one canonical Session path |
| Session contract and Managed Session store | add the `DreamProcessStore` port plus scoped process/policy tables and SQLite/Postgres CAS adapters | Dream durability belongs to Coordinator rather than the Memory extension or protocol adapter |
| server assembly | FUSE-preferred Memory mounter and one Dream composition root | ordinary mounts may fall back; Dream output may not |
| Managed periodic driver | evaluate opt-in Dream policies beside cron Deployments | one timer and one trigger path; policies never execute an Agent directly |

### Genuinely new behavior

| New type/component | Owner | Responsibility |
|---|---|---|
| `DreamApplication` / `DreamProcess` | Coordinator application | validation, durable lifecycle, filtering, cancellation, archive, public projection |
| Dream DTOs and routes | Managed protocol adapter | Anthropic-compatible HTTP and SDK shape |
| `DreamExecutor` | application port | the single orchestration seam between lifecycle and lower authorities |
| `BuiltInDreamAgent` | server composition | snapshots inputs, exports JSONL, contributes an ordinary Session, executes and cleans up |
| `SessionTranscriptJsonlExporter` | server composition | deterministic committed-Message JSONL encoding |
| `MemoryStoreContentSnapshot` | server composition | names one exact set of source heads used for both input and output |
| `ExclusiveMemoryStoreWriterLease` | server composition | keeps the result catalog-gated until the auxiliary writer terminates |

No `DreamAgentState`, `MemoryCompactAgent`, alternate transcript schema store, or
parallel output import/harvest implementation exists.

## Role / Component Catalog

The three ownership tables above are the catalog: they identify every reused,
modified, and new component, its owner, and its bounded responsibility. The
following static view shows how those roles depend on one another.

## Static structure

```text
Dream protocol routes (DTO/error mapping only)
    |
    v
DreamApplication ------ DreamProcessStore port
                                |
                                `------ Sqlite/PostgresManagedSessionRepository
    |
    v  DreamExecutor
BuiltInDreamAgent
    |---- ResourceCatalog: source validation
    |---- MemoryStoreApplicationService: output identity/lifecycle fence
    |---- MemoryRepository.snapshot_heads: frozen source and independent clone
    |---- ManagedState: committed transcript + ordinary auxiliary Session
    |---- SharedHost Files: transient JSONL evidence
    `---- provisioning: RO input/transcripts + strict write-through output
                    |
                    v
              SessionRuntime / normal Run
```

The data contracts are deliberately narrow:

```rust
struct DreamRequest {
    job_id: String,
    workspace_id: String,
    source_memory_store_id: String,
    session_ids: Vec<String>,
    model: DreamModelConfig,
    request_guidance: Option<String>,
    agent_id: String,
}

struct DreamPreparation {
    result_memory_store_id: String,
    session_id: String,
    transcript_file_ids: Vec<String>,
}
```

`DreamProcessStore` is the only source for preparation/cleanup phase, terminal
decision, archive time, and output/Session references. The linked Session is the
only source for cumulative token usage; the Dream record never stores a copy.
Memory content, File bytes, and ordinary Session events stay owned by their
existing repositories.

## Managed Agents-compatible API

Dream routes require both capabilities:

```text
anthropic-beta: managed-agents-2026-04-01,dreaming-2026-04-21
```

SDK methods map directly to:

| SDK operation | HTTP operation |
|---|---|
| `client.beta.dreams.create(...)` | `POST /v1/dreams` |
| `client.beta.dreams.retrieve(id)` | `GET /v1/dreams/{id}` |
| `client.beta.dreams.list(...)` | `GET /v1/dreams` |
| `client.beta.dreams.cancel(id)` | `POST /v1/dreams/{id}/cancel` |
| `client.beta.dreams.archive(id)` | `POST /v1/dreams/{id}/archive` |

The SDK may append `?beta=true`; capability authorization remains
header-driven. Create accepts exactly one MemoryStore input, exactly one
Sessions input, 1–100 unique Session ids, and at most 4096 instruction
characters:

```json
{
  "inputs": [
    {"type": "memory_store", "memory_store_id": "mem_01"},
    {"type": "sessions", "session_ids": ["sesn_01", "sesn_02"]}
  ],
  "model": {"id": "claude-sonnet-5", "speed": "standard"},
  "instructions": "Prefer durable project conventions."
}
```

The supported model ids follow the current Dreams documentation:
`claude-fable-5`, `claude-opus-4-8`, `claude-opus-4-7`,
`claude-sonnet-5`, and `claude-sonnet-4-6`. The response has `type: "dream"`,
the original inputs/model/instructions, lifecycle timestamps, error, usage,
`outputs[]`, and the auxiliary `session_id`. Output and Session references appear
after preparation commits and remain present on failure or cancellation.
`speed: "fast"` is rejected at create time because the installed inference
adapter does not support that option; it is never accepted and silently ignored.

List is newest-first, defaults to 20, caps `limit` at 100, excludes archived
jobs by default, and supports `page`, repeated `statuses`, `include_archived`,
`created_at[gt]`, and `created_at[lt]`.

## Ordinary Dream Agent configuration

Dream has no special configuration contract. Its stable `agent_id` is frozen in
the durable job exactly like any other Agent reference:

```text
published awaken_builtin_dream_agent -> use that complete executable snapshot
publication absent                    -> use the built-in fallback snapshot
```

Users customize it through the ordinary Agent draft/publish UI and API. There is
no `/v1/dream_agent_configuration`, Workspace override table, Dream-specific
prompt store, or second catalog. Request `model` remains the Anthropic Dream
request field, while request `instructions` is bounded per-job guidance and
cannot widen mounts, tools, network, or credentials.

## Automatic Dream policy

The scheduling key is exactly `(workspace_id, memory_store_id)`. Absence is the
effective default and means disabled; no policy row or default Agent row is copied
for each Workspace. A configured policy contains:

```text
enabled                 default false
interval_seconds        minimum 60; recommended/default 86400
min_new_sessions        default 5
max_sessions            default/cap 100
model + instructions    same validation as manual Dream create
next_due_ms              durable cursor
last_completed_cutoff_ms durable successful-evidence cutoff
```

At a due tick, the Session source selects the most recently updated non-running, non-Dream auxiliary
Sessions in the Workspace whose `updated_at` is newer than the last successful
cutoff. If fewer than `min_new_sessions` exist, only `next_due_ms` advances. If a
job for the same policy is already pending/running, no duplicate is submitted.
Otherwise the selected set is capped at `max_sessions`. Advancing the exact
policy version and inserting its ordinary DreamProcess is one repository transaction,
so multiple scheduler replicas cannot claim the same occurrence. Only successful completion advances the evidence cutoff;
failure/cancel therefore permits a later retry over the same evidence.

Deployment cron and Dream interval policies intentionally have different domain
contracts but one production driver. Deployment creates ordinary Sessions from a
cron occurrence; Dream policy creates a DreamProcess, which then creates its bounded
auxiliary Session. Reusing the timer does not conflate these aggregates.

Dream scheduling policy is an internal Coordinator application contract, not a
Managed protocol method. No `/v1/dream_policies/*` route is mounted. If policy
authoring is exposed later, it must be a separately named Control/Awaken API that
submits this same Coordinator command; it may not be added to
`awaken-protocol-managed`.

## Frozen inputs and JSONL

`MemoryRepository::snapshot_heads` obtains one atomic, path-ordered set of
current source file heads. The worker uses those exact bytes twice: once to
build a hidden read-only snapshot store and once to initialize the independent
result store. Later source writes cannot change either copy.

For each selected Session, `ManagedState` returns the complete committed Message
sequence available at export time. One line is emitted per Message:

```json
{"type":"committed_message","session_id":"sesn_01","ordinal":0,"message":{"id":"msg_01","role":"user","content":[{"type":"text","text":"..."}]}}
```

The serialized `message` is the canonical Message value. Consequently committed
tool-use inputs and tool-result payloads are retained in order; uncommitted
streaming deltas and provider-hidden data are not part of this read model and
are not exported. An empty Session produces an empty JSONL file.

Exports are canonical transient Files artifacts (`application/x-ndjson`) and are mounted
as `InlineBytes` under `/mnt/dream/session-transcripts`. They are not injected
into the initial model context, so the Agent can `Glob`, `Grep`, and `Read` only
the evidence it needs. Their File ids are stored with the durable preparation and
deleted through the ordinary File lifecycle after terminal/cancel cleanup; a
crash leaves `cleanup_pending` so restart retries that same cleanup.

## Mount and Agent capability contract

The ordinary Session receives exactly these required mounts:

| Path | Source | Access/consistency |
|---|---|---|
| `/mnt/dream/input-memory` | hidden frozen MemoryStore | read-only |
| `/mnt/dream/session-transcripts/<session>.jsonl` | exported immutable bytes | read-only |
| `/mnt/dream/output-memory` | independent result MemoryStore | read-write, `WriteThroughRequired` |

`WriteThroughRequired` expresses the semantic requirement. Today a provider
satisfies it with `Realization::Fuse`; copy realization is torn down and the Run
fails before Agent launch. Ordinary non-Dream Memory mounts retain their existing
copy fallback.

The built-in Session disables every tool by default and enables only
`read`, `write`, `edit`, `glob`, and `grep`; network policy is `None`. Writes are
therefore limited to the only writable mount. Rename/delete and MCP, shell, delegation, credential
access, extraction, and nested Dream capabilities are absent.

The immutable platform prompt directs an evidence-first sequence:

1. read the frozen input Memory and relevant JSONL;
2. preserve durable facts, decisions, preferences, constraints, and unresolved work;
3. merge into existing topic files and remove duplicates;
4. correct facts contradicted by newer evidence and normalize relative dates;
5. keep `MEMORY.md` a concise index of at most 200 lines;
6. write only Markdown under the output mount and never promote secrets or personal memory.

Prompt text is defense in depth; mount access, tool configuration, network
policy, and write-through enforcement are the authorities.

## Dynamic behavior

### Create, execute, and recover

```text
create
  -> validate shape/model/resources and freeze Agent selection
  -> persist Pending job
  -> return immediately and spawn work

worker
  -> persist Running
  -> derive deterministic snapshot/result/Session ids from Dream id
  -> reclaim an uncommitted partial clone, or reuse an existing prepared result
  -> atomically snapshot source; clone snapshot and result
  -> Resources application creates result as Suspended (exclusive writer fence)
  -> export JSONL Files
  -> create/realize ordinary auxiliary Session with strict mounts
  -> persist output and Session references
  -> execute one idempotently identified ordinary user event
  -> revalidate every input (archive/delete during execution is a typed failure)
  -> durably record terminal outcome with cleanup_pending
  -> archive auxiliary Session, activate result, purge hidden snapshot and JSONL Files
  -> clear cleanup_pending; only now publish Completed or Failed
```

Jobs, policies, and Workspace overrides use the exact Session-store backend
selected by the production composition root: `SqliteManagedSessionRepository`
or `PostgresManagedSessionRepository`. Startup CAS-resets durable `pending`/`running`
jobs to `pending` and
redispatches them. Deterministic lower-resource ids make preparation idempotent:
an existing result and Session are adopted, and an already committed Dream trigger
is not sent twice; a clone left before its catalog
commit is purged and rebuilt. Terminal jobs are loaded without redispatch.
Terminal jobs with `cleanup_pending` redispatch cleanup only.

Dream policies and their due/success cursors are stored in the same repository.
The composition root's single 15-second Managed timer first claims due Deployment
occurrences and then evaluates due Dream policies. Either failure is reported and
does not stop the other family on later ticks.

### Cancel and archive

Cancel immediately and durably sets a pending/running job to `canceled`, sets
`ended_at`, signals the worker, interrupts and archives an existing auxiliary
Session, releases the result availability fence, and retains the result store.
Repeated cancel is idempotent. Canceling `completed` or `failed` returns 400;
late worker completion cannot overwrite `canceled`.

Archive is terminal-only and idempotent. It sets `archived_at` without changing
status and does not delete the output MemoryStore or auxiliary Session history.
Transient JSONL Files have already been purged at the terminal cleanup boundary.

## Failure and consistency boundaries

- Source Memory and transcript mounts are read-only; result is the only writable mount.
- Source and result have independent ids and file histories.
- The same atomic Memory snapshot initializes both frozen input and result.
- A result mount is write-through or the Agent never starts.
- Failed/canceled Dreams retain a result that was successfully prepared and all writes already committed through it.
- Dream state, Memory content, File bytes, and Session events each have one non-overlapping authority.
- SQLite/Postgres transitions use exact-version CAS; persistence failure or a concurrent replica never creates a second in-memory authority.
- Workspace ownership is checked without revealing cross-Workspace resources.

Public execution error types are `timeout`, `internal_error`,
`memory_store_org_limit_exceeded`, `input_memory_store_too_large`,
`input_memory_store_unavailable`, and `input_session_unavailable`.
Awaken enforces a private 10 MiB aggregate input-Memory ceiling and a private
six-hour execution budget so those two documented terminal kinds have concrete,
finite self-hosted causes. Anthropic does not publish its hosted byte threshold
or runtime budget, so those numeric values are deployment policy rather than a
claim of hosted equality.

## Test design and coverage

Cause/effect design is recorded beside each corresponding test, as required;
this summary explains the bounded coverage without becoming a separate test
oracle.

| Rule | Causes | Effects / owning test |
|---|---|---|
| API-1 | valid create and successful/failed worker | compatible projection, usage, error, partial output; `official_create_retrieve_list_archive_and_failure_shapes` |
| API-2 | invalid input cardinality/count/uniqueness/instructions/model/resource | 400 and no valid job; `validation_and_terminal_mutation_decision_table` |
| API-2b | input archived/deleted after create | typed failure before terminal publication; `input_deleted_after_create_fails_before_terminal_publication` |
| API-3 | running job cancel/repeat/late completion | immediate idempotent canceled, output retained, no overwrite; `cancellation_is_immediate_idempotent_and_retains_prepared_output` |
| API-3b | canceled job whose ordinary Session usage advances while in-flight work winds down | status remains canceled while later retrieval projects the newer usage; `canceled_dream_keeps_projecting_trailing_session_usage` |
| API-4 | neither/one/both beta capabilities | only both reach Dream route; `dream_routes_require_managed_and_dreaming_betas` |
| API-5 | limit/cursor/repeated status/date bounds | SDK-compatible newest-first pages or 400; `list_supports_official_repeated_status_filters_and_cursor_pages` |
| API-6 | Running before/after durable preparation | `outputs[]` and `session_id` transition from empty/null to the prepared references without leaving Running; `running_output_projection_transitions_from_empty_to_prepared` |
| ERR-1 | preparation or execution exceeds the one application runtime budget | exact failed `timeout`, retained prepared output when present, and one cleanup owner; `runtime_budget_fails_prepare_or_execution_with_one_cleanup_owner` |
| ERR-2 | aggregate input Memory bytes at/over the private pipeline ceiling | boundary accepted or `input_memory_store_too_large` before output/Session writes; `dream_input_memory_limit_maps_the_exact_pipeline_error` |
| ERR-3 | output-store organization cap, timeout, or oversized input reaches the HTTP adapter | exact failed Dream error union with no invented output/session; `documented_pipeline_failures_round_trip_through_http` |
| REC-1 | terminal SQLite process and ordinary Session facts after restart | same status/output plus Session-derived usage; `sqlite_repository_restores_terminal_dreams_after_restart` |
| REC-2 | process loss during Running | CAS reset and canonical redispatch; `resume_incomplete_cas_resets_and_executes_a_durable_running_job` |
| REC-3 | terminal decision plus cleanup failure | public Running until cleanup-only restart succeeds; `restart_retries_terminal_cleanup_before_publishing_completion` |
| SEL-1 | manual/scheduled Dream creation | stable ordinary Agent id frozen directly on every process, with no Dream-specific selection state; `dream_uses_one_stable_ordinary_agent_id_without_selection_state` |
| SCH-1 | disabled / below threshold / due / completed cutoff | no process, threshold gating, one ordinary DreamProcess, no unchanged reprocessing; `automatic_policy_is_opt_in_thresholded_and_reuses_the_dream_job_path` |
| SCH-2 | absent / invalid / configured / restart | default projection, atomic 400, durable cursor/config restore; `dream_policy_api_projects_defaults_validates_and_survives_restart` |
| SCH-3 | two replicas claim one due policy version | one atomic policy/job winner and one benign loser; `concurrent_policy_ticks_claim_one_dream_across_replicas` |
| MEM-1 | snapshot then source update | frozen heads unchanged and path ordered; `snapshot_heads_conformance` |
| MNT-1 | strict output with copy-only mounter | teardown and fail before Agent launch; `write_through_required_rejects_copy_before_agent_launch` |
| JSONL-1 | committed text/tool-use/tool-result/empty history | exact ordered payload or empty file; `jsonl_export_preserves_every_committed_message_and_tool_payload_in_order` |
| E2E-1 | Agent + Session + Events + Files + MemoryStore + Dream | one runtime/data plane, real restricted write tool call, transient JSONL cleanup, ordinary auxiliary Session, independent output/source; `agent_session_events_files_memory_and_dream_share_one_runtime_and_data_plane` |
| E2E-2 | official TypeScript SDK + Session + Files + MemoryStore + Dream | typed SDK creation/poll/list/cancel/archive across one process boundary; `managed_dream_e2e.ts` |

### P0-P6 completion evidence

The P0-P6 labels are test-work packages, not seven parallel implementations.
Each package terminates in the same `DreamApplication` / `DreamExecutor` path described
above, and the cause/effect table in `managed_dream_e2e.ts` is its executable
cross-module acceptance design.

| Package | Required outcome | Authoritative evidence |
|---|---|---|
| P0 | current official SDK can create and decode a typed asynchronous Dream with the Managed and Dreaming capabilities | SDK `0.115.0`; `managed_dream_e2e.ts`; SDK-surface conformance gate |
| P1 | one frozen source and selected committed Sessions produce a terminal Dream with stable output and auxiliary Session references | `DreamApplication`; `BuiltInDreamAgent`; Rust and TypeScript Dream E2Es |
| P2 | each committed transcript is exported as valid JSONL, remains file input read on demand during execution, and is purged afterward | `SessionTranscriptJsonlExporter`; `jsonl_export_preserves_every_committed_message_and_tool_payload_in_order`; transient Files assertion |
| P3 | input Dream store is read-only and unchanged; output Dream store is a distinct clone and the sole strict write-through target | `MemoryStoreContentSnapshot`; `WriteThroughRequired`; mount decision-table tests; both cross-module E2Es |
| P4 | Dream execution is an ordinary restricted auxiliary Session, not an alternate Agent state | `ManagedState`; `BuiltInDreamAgent`; auxiliary Session origin/terminal assertions |
| P5 | retrieve/list/filter/cancel/archive, durable recovery, Workspace selection, automatic policy, and shared periodic scheduling obey their state rules | protocol cause/decision-table tests; SQLite recovery tests; policy tests |
| P6 | the public contract crosses Session, MemoryStore, Dream, and Files modules through the official TypeScript SDK without a Dream-specific shim | `managed_dream_e2e.ts`; `sdk_surface_coverage_e2e.mjs` |

## Required versus deferred work

Implemented required behavior is the explicit Anthropic-compatible Dream API,
durable SQLite/Postgres lifecycle/recovery, frozen JSONL evidence, independent Memory
output, strict mount semantics, auxiliary Agent execution, Workspace selection,
and cross-module E2E.

Deferred product behavior is automatic replacement of an Agent's bound MemoryStore,
personal-to-team promotion, and multi-source Memory merge. Those features
must submit or store the same `DreamProcess`; they may not add another executor or
state machine.

Anthropic-hosted synthesis quality over real minute-to-hour workloads, billed
token totals, and service-side rate-limit enforcement are external acceptance
evidence. Fake providers and self-hosted limits can prove deterministic protocol,
state, and error semantics, but must not be cited as proof of Anthropic billing,
quality, availability, or operator quotas.

## References

- [Anthropic Managed Agents overview](https://platform.claude.com/docs/en/managed-agents/overview)
- [Anthropic Dreams](https://platform.claude.com/docs/en/managed-agents/dreams)
- [Anthropic TypeScript SDK Dreams resource](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/beta/dreams.ts)
- [Auxiliary Context Windows](auxiliary-context-windows.md)
- [Resources, Memory, Files, And Skills](resources-memory-files-skills.md)
- [ADR-0053: Memory Store as a Write-Through FUSE Mount](../adr/0053-memory-store-fuse-mount.md)
- [Managed Agents Runtime Protocol Adapter](anthropic-alignment-and-sessions.md)
