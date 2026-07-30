# Managed Dream Memory Consolidation

## End-to-end objective

Anthropic exposes this asynchronous resource as a **Dream**. Awaken names the
internal operation **Memory Consolidation**: it freezes one source MemoryStore
and the committed histories of 1–100 Sessions, exports each transcript as JSONL,
and runs an ordinary auxiliary Agent against a new, independent MemoryStore.

```text
POST /v1/dreams
  -> durable MemoryConsolidationJob
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
Session retains the ordinary Session/Run lifecycle; `MemoryConsolidationJob`
owns only the public Dream lifecycle and references that Session.

Automatic scheduling is intentionally separate and deferred. Every Workspace
has an effective consolidator (built-in unless explicitly overridden), but that
default does not automatically create Dreams.

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
| ordinary Agent execution | `awaken-runtime-host::SessionRuntime` | executes the consolidator through the normal Run path |
| Memory file truth | `awaken-resource-contract::MemoryRepository` | reads and commits path-addressed Memory content |
| Files data plane | `awaken-server::SharedHost` | stores downloadable JSONL artifacts using the canonical File record path |
| Resource catalog | `ResourceCatalog` | validates source ownership and gates the output store while Dream owns it |
| mount realization | provisioning and sandbox providers | realizes InlineBytes and MemoryStore inputs for the ordinary Session |

### Modified existing owners

| Owner | Change | Invariant gained |
|---|---|---|
| `MemoryRepository` and its volatile/SQLite/Postgres implementations | `snapshot_heads` | one backend-atomic, path-ordered source view |
| provisioning vocabulary | `MemoryWriteConsistency::{ProviderDefault, WriteThroughRequired}` | callers can require live Memory writes without naming FUSE |
| local, namespace, and container providers | reject copy realization for `WriteThroughRequired` | no silent copy/harvest downgrade before Agent launch |
| Managed beta middleware and rate-limit classifier | Dreams family and both beta capabilities | wire compatibility and existing Managed request governance |
| Managed Session application | exact built-in-origin exception and public realization seam | the built-in auxiliary Agent still uses one canonical Session path |
| Memory extension and Managed Session store | add the `MemoryConsolidationRepository` port plus scoped job/override tables | Dream durability reuses the auxiliary-work repository family instead of embedding a database in the protocol adapter |
| server assembly | FUSE-preferred Memory mounter and one Dream composition root | ordinary mounts may fall back; Dream output may not |

### Genuinely new behavior

| New type/component | Owner | Responsibility |
|---|---|---|
| `DreamState` / `MemoryConsolidationJob` | Managed protocol application | validation, durable lifecycle, filtering, cancellation, archive, public projection |
| Dream DTOs and routes | Managed protocol adapter | Anthropic-compatible HTTP and SDK shape |
| `MemoryConsolidationWorker` | application port | the single orchestration seam between lifecycle and lower authorities |
| `BuiltInMemoryConsolidatorAgent` | server composition | snapshots inputs, exports JSONL, contributes an ordinary Session, executes and cleans up |
| `SessionTranscriptJsonlExporter` | server composition | deterministic committed-Message JSONL encoding |
| `MemoryStoreContentSnapshot` | server composition | names one exact set of source heads used for both input and output |
| `ExclusiveMemoryStoreWriterLease` | server composition | keeps the result catalog-gated until the auxiliary writer terminates |
| `MemoryConsolidationAgentSelection` | Managed protocol application | freezes the effective built-in or Workspace override at create time |

No `DreamAgentState`, `MemoryCompactAgent`, alternate transcript schema store, or
parallel output import/harvest implementation exists.

## Role / Component Catalog

The three ownership tables above are the catalog: they identify every reused,
modified, and new component, its owner, and its bounded responsibility. The
following static view shows how those roles depend on one another.

## Static structure

```text
Dream routes
    |
    v
DreamState ------ MemoryConsolidationRepository port
                          |
                          `------ SqliteManagedSessionRepository
    |
    v  MemoryConsolidationWorker
BuiltInMemoryConsolidatorAgent
    |---- ResourceCatalog: source validation + output availability fence
    |---- MemoryRepository.snapshot_heads: frozen source and independent clone
    |---- ManagedState: committed transcript + ordinary auxiliary Session
    |---- SharedHost Files: downloadable JSONL evidence
    `---- provisioning: RO input/transcripts + strict write-through output
                    |
                    v
              SessionRuntime / normal Run
```

The data contracts are deliberately narrow:

```rust
struct MemoryConsolidationRequest {
    job_id: String,
    workspace_id: String,
    source_memory_store_id: String,
    session_ids: Vec<String>,
    model: DreamModelConfig,
    request_guidance: Option<String>,
    agent_selection: MemoryConsolidationAgentSelection,
}

struct MemoryConsolidationPreparation {
    result_memory_store_id: String,
    session_id: String,
}
```

`MemoryConsolidationJob` is the only source for `pending`, `running`,
`completed`, `failed`, `canceled`, archive time, output id, usage, and error.
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

List is newest-first, defaults to 20, caps `limit` at 100, excludes archived
jobs by default, and supports `page`, repeated `statuses`, `include_archived`,
`created_at[gt]`, and `created_at[lt]`.

## Workspace consolidator selection

There is one effective consolidator per Workspace, not one copied Agent record:

```text
Workspace override absent  -> awaken_builtin_memory_consolidator
Workspace override present -> validate and use that exact active Agent
```

Selection is frozen into the job before dispatch. Changing the Workspace policy
therefore affects only later Dreams. Request `model` overrides the selected
Agent's model for this Session, while request `instructions` is appended as
bounded guidance and cannot widen mounts, tools, network, or credentials.

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

Exports are canonical Files artifacts (`application/x-ndjson`) and are mounted
as `InlineBytes` under `/mnt/dream/session-transcripts`. They are not injected
into the initial model context, so the Agent can `Glob`, `Grep`, and `Read` only
the evidence it needs.

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
therefore limited to the only writable mount. MCP, shell, delegation, credential
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
  -> catalog result as Suspended (exclusive Dream writer fence)
  -> export JSONL Files
  -> create/realize ordinary auxiliary Session with strict mounts
  -> persist output and Session references
  -> execute one ordinary user event
  -> archive auxiliary Session, activate result, purge hidden snapshot
  -> persist Completed and usage, or Failed and error
```

With host storage configured, jobs and Workspace overrides are stored beside the
ordinary Session aggregate in `sessions.db` through the canonical
`SqliteManagedSessionRepository`. Startup resets durable `pending`/`running`
jobs to `pending` and
redispatches them. Deterministic lower-resource ids make preparation idempotent:
an existing result and Session are adopted; a clone left before its catalog
commit is purged and rebuilt. Terminal jobs are loaded without redispatch.

### Cancel and archive

Cancel immediately and durably sets a pending/running job to `canceled`, sets
`ended_at`, signals the worker, interrupts and archives an existing auxiliary
Session, releases the result availability fence, and retains the result store.
Repeated cancel is idempotent. Canceling `completed` or `failed` returns 400;
late worker completion cannot overwrite `canceled`.

Archive is terminal-only and idempotent. It sets `archived_at` without changing
status and does not delete the output MemoryStore, JSONL Files, or auxiliary
Session history.

## Failure and consistency boundaries

- Source Memory and transcript mounts are read-only; result is the only writable mount.
- Source and result have independent ids and file histories.
- The same atomic Memory snapshot initializes both frozen input and result.
- A result mount is write-through or the Agent never starts.
- Failed/canceled Dreams retain a result that was successfully prepared and all writes already committed through it.
- Dream state, Memory content, File bytes, and Session events each have one non-overlapping authority.
- SQLite transition persistence occurs after each visible lifecycle change; persistence failure never creates a second in-memory authority.
- Workspace ownership is checked without revealing cross-Workspace resources.

Public execution error types are `timeout`, `internal_error`,
`memory_store_org_limit_exceeded`, `input_memory_store_too_large`,
`input_memory_store_unavailable`, and `input_session_unavailable`.

## Test design and coverage

Cause/effect design is recorded beside each corresponding test, as required;
this summary explains the bounded coverage without becoming a separate test
oracle.

| Rule | Causes | Effects / owning test |
|---|---|---|
| API-1 | valid create and successful/failed worker | compatible projection, usage, error, partial output; `official_create_retrieve_list_archive_and_failure_shapes` |
| API-2 | invalid input cardinality/count/uniqueness/instructions/model/resource | 400 and no valid job; `validation_and_terminal_mutation_decision_table` |
| API-3 | running job cancel/repeat/late completion | immediate idempotent canceled, output retained, no overwrite; `cancellation_is_immediate_idempotent_and_retains_prepared_output` |
| API-4 | neither/one/both beta capabilities | only both reach Dream route; `dream_routes_require_managed_and_dreaming_betas` |
| API-5 | limit/cursor/repeated status/date bounds | SDK-compatible newest-first pages or 400; `list_supports_official_repeated_status_filters_and_cursor_pages` |
| REC-1 | terminal SQLite job and restart | same status/output/usage restored; `sqlite_repository_restores_terminal_dreams_after_restart` |
| SEL-1 | default/override Workspace policy | effective selection frozen without copied default rows; `workspace_agent_selection_uses_effective_default_and_freezes_override` |
| MEM-1 | snapshot then source update | frozen heads unchanged and path ordered; `snapshot_heads_conformance` |
| MNT-1 | strict output with copy-only mounter | teardown and fail before Agent launch; `write_through_required_rejects_copy_before_agent_launch` |
| JSONL-1 | committed text/tool-use/tool-result/empty history | exact ordered payload or empty file; `jsonl_export_preserves_every_committed_message_and_tool_payload_in_order` |
| E2E-1 | Agent + Session + Events + Files + MemoryStore + Dream | one runtime/data plane, JSONL readable on demand, ordinary auxiliary Session, independent output/source unchanged; `agent_session_events_files_memory_and_dream_share_one_runtime_and_data_plane` |

## Required versus deferred work

Implemented required behavior is the explicit Anthropic-compatible Dream API,
durable local lifecycle/recovery, frozen JSONL evidence, independent Memory
output, strict mount semantics, auxiliary Agent execution, Workspace selection,
and cross-module E2E.

Deferred work is periodic auto-Dream scheduling, automatic replacement of an
Agent's bound MemoryStore, personal-to-team promotion, multi-source Memory merge,
and a distributed Dream repository for a coordinator-only deployment. Those
features must submit or store the same `MemoryConsolidationJob`; they may not add
another executor or state machine.

## References

- [Anthropic Managed Agents overview](https://platform.claude.com/docs/en/managed-agents/overview)
- [Anthropic Dreams](https://platform.claude.com/docs/en/managed-agents/dreams)
- [Anthropic TypeScript SDK Dreams resource](https://github.com/anthropics/anthropic-sdk-typescript/blob/main/src/resources/beta/dreams.ts)
- [Auxiliary Context Windows](auxiliary-context-windows.md)
- [Resources, Memory, Files, And Skills](resources-memory-files-skills.md)
- [ADR-0053: Memory Store as a Write-Through FUSE Mount](../adr/0053-memory-store-fuse-mount.md)
- [Managed Agents Runtime Protocol Adapter](anthropic-alignment-and-sessions.md)
