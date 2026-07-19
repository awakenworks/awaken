# Cell-Based Distributed ACP Execution

Single `server` owns SQLite, workers are stateless and converge (not relay), the
ACP stream never crosses the network, and Kubernetes scales workers. One **cell**
is self-contained; scale by replicating cells keyed on `thread_id`. Everything
lands on awaken's existing seams (`RunExecutor`, `RunIngress`/`DispatchQueue`,
`AgentEvent`, `SharedHost`, `awaken-run-executor-acp`).

- Status: Proposed
- Date: 2026-07-13
- Builds on: [run-ingress-message-delivery](run-ingress-message-delivery.md)
  (the Dispatch/Server boundary; `thread_id` is the shard and consistency key;
  only one owner may freeze/snapshot/execute a thread); the ACP `RunExecutor`
  turn model (`awaken-run-executor-acp`, `awaken-protocol-acp`); durable dispatch
  and lease recovery (`awaken-run-ingress`, `SharedHost::ensure_dispatch_pool`).

## Verdict

**Cell-based + phased.** A cell is `{ one server (data-plane hub, commits on
relay) + one local SQLite (single writer) + a fleet of Kubernetes workers
(multiplex many runs, drive ACP locally) }`. Run a single cell to its ceiling
first, then shard into multiple cells keyed on `thread_id`. **Do not build
sharding or failover yet.**

## Why this shape

The whole design follows one fact: **the real load is not "who dispatches whom",
it is "where the execution output is written".**

Dispatch (claim/settle) is control-plane — a handful of messages per run, ignore
it. The raw ACP stream is a firehose, but it is **stdio, in-pod, driven locally,
and never crosses the network**. The only real cross-network load is the one
where a worker **projects at the edge** the raw stream into neutral `AgentEvent`
facts and pushes them to the server, which commits them as it forwards. Shard and
localize that write path and the architecture scales — and SQLite survives as
"one local database per cell".

## Design principles (settled; invariant across scale)

| Principle | What it means |
|---|---|
| **Server in the data path** | The server is the sole writer; it commits the edge-projected facts as it forwards them. The single-writer SQLite model holds natively, and it dissolves the "how does a cross-node worker write the DB" problem — the worker does not write, the server does. |
| **Worker multiplexes runs** | One worker drives hundreds of ACP CLIs asynchronously (I/O-bound), not pod-per-run. Density buys cost and throughput. |
| **ACP local + edge projection** | Raw ACP stdio stays in the pod; the worker converges the token stream into neutral `AgentEvent` before it crosses the network. Facts cross the hub, not every token. |
| **Egress local** | Agent tool egress goes out from the worker locally with policy enforced in place — **never proxied through the hub**. |
| **k8s scales workers only** | Kubernetes schedules pods, not runs. HPA scales workers; the dispatch protocol distributes runs. Complementary. |
| **Swappable store per cell** | Because the server is the sole writer, a cell whose SQLite tops out on writes can be moved to Postgres independently, without touching other cells. |

## The worker is convergence + a trust gate, not a relay

An ACP CLI only speaks ACP over stdio; it does not know the server. Something must
drive ACP, **converge** the firehose into facts, and hold the credential. That
work cannot be removed — only placed. That is the worker. It does four things a
relay does not: **actively drive ACP · firehose → facts (an order of magnitude
smaller) · hold the server credential (the untrusted CLI never touches it) ·
fan many runs into few connections.**

**Co-location spectrum** — where the driver sits decides isolation vs multiplexing:

| Co-location | Form | Isolation | Multiplexing / fan-in collapse | Fit |
|---|---|---|---|---|
| **Same process** | worker fork-execs child sandbox (`SubprocessChannelSource` / sandbox provider) | process / namespace | strong (one worker drives hundreds) | max density |
| **Same pod (sidecar)** | CLI container + driver sidecar | container-level | none — pod-per-run | small/medium scale |
| **Same node (DaemonSet)** | per-node driver + many sandbox pods, ACP over a node-local unix socket | container-level | one daemon multiplexes the whole node | large scale + isolation |

The trust boundary is always: **untrusted CLI ──stdio (speaks only ACP)──▶ trusted
worker (holds the credential) ──network──▶ server.** A sidecar realizes it with a
container boundary; a node-daemon keeps that isolation while regaining
multiplexing.

## Data flows and their magnitude

Five flows, orders of magnitude apart. The entire point is to keep the firehoses
(②④) local and let only facts (③) cross the sharded hub, while control-plane (①)
stays light.

```
  Client  ⇄  Cell Server  ⇄  Worker  ⇄  Sandbox (ACP CLI + tools)
    │  ⑤ SSE     │  ① dispatch  │  ② ACP     │  ④ egress → external
    │  submit    │  ③ facts     │  stdio     │  (local direct-out)
    └────────────┴──────────────┴────────────┘
                 │
          SQLite (this cell)  ◀── sole writer = server
          dispatch queue + committed truth
```

| Flow | Path | Magnitude | Where | Note |
|---|---|---|---|---|
| **① dispatch** | server ⇄ worker | low | network | claim/settle, a few per run. Control-plane; keep it simple. |
| **② ACP stream** | worker ⇄ CLI | very high | **local stdio** | firehose. Pinned in the pod, never on the network. |
| **③ facts** | worker → server | medium | network | projected `AgentEvent`. **The real load — this is what shards.** |
| **④ egress** | worker → external | high | **local direct-out** | policy in place, never proxied through the hub. |
| **⑤ client subscribe** | client ⇄ server | low-med | network | facts fan out on relay; commit and relay co-located. |

## Protocol view — a narrow waist, not "one protocol for all"

Each hop speaks its own protocol (ACP/stdio, HTTP, SSE, MCP, A2A). Reuse across
protocols comes from **narrowing to one neutral waist**: two projections in
opposite directions turn N×M into N+M.

```
 agent protocols            NEUTRAL WAIST              frontend wires
 (write side, converge)                                (read side, adapt)

 AcpRunExecutor  ──┐                              ┌── protocol-managed
 A2aRunExecutor  ──┼─▶  RunActivation (self-      ┼── protocol-ai-sdk
 native executor ──┘     contained) + AgentEvent  └── protocol-ag-ui
    (one RunExecutor       + commit_run               (one adapter
     impl per protocol)    (transport-agnostic)        per frontend)
```

- **Add an agent protocol** = one `RunExecutor` impl + one projection; nothing
  in transport / store / other protocols changes.
- **Add a frontend** = one `awaken-protocol-*` adapter; the N agent protocols are
  untouched.
- **Selection by backend-ref**: `Backend::from_ref` → `acp:…` / `a2a:…` / native.
- Neutral facts are the **committed durable truth**, so a run started over ACP is
  replayable to any frontend (the cross-protocol invariant).

## The event sink is a multiplexed per-thread channel

The worker→server facts channel (③) is **not one connection per run** — one
connection carries every run the worker drives, each fact tagged
`(thread_id, run_id, seq)`. The server demultiplexes, commits per thread, and
orders by `seq` (the `AppendError::NonMonotonic` guard). This is the connection
fan-in collapse (tens of thousands of connections instead of tens of millions).

- **Multiplex within a cell**: a worker is bound to one cell; its threads are all
  in that cell, so the sink never crosses cells.
- **Per-thread ordering**: threads are independent aggregates; only each `seq`
  must increase.
- **Batch/stream**: accumulate or window before sending — **not one POST per
  token**. The magnitude of ③ lives or dies here.

## Deployment — Phase 1: single cell

```
┌──────────────── Kubernetes Namespace — one cell ─────────────────┐
│                                                                  │
│   Ingress / LB ──▶ Cell Server (SharedHost)      Worker Deploy   │
│                    StatefulSet · 1 writer         HPA · N replicas│
│                    commits on relay               ┌────────────┐ │
│                         │                         │ worker-1   │ │
│                    PVC → sqlite                    │  sandbox×M │ │
│                    dispatch + committed truth      │  ACP·stdio │ │
│                         ▲                          ├────────────┤ │
│              ① dispatch ⇅  ③ facts ⇅ (HTTP)        │ worker-N   │ │
│                                                    │  sandbox×M │ │
│                                                    └────────────┘ │
│              ④ egress out of each worker locally ─────▶ external  │
└──────────────────────────────────────────────────────────────────┘
```

- **StatefulSet** cell-server (`SharedHost`) — 1 replica (single writer) + **PVC**
  mounting `sqlite`; a **Service**.
- **Deployment** worker — N replicas + **HPA** (scale on in-flight run count /
  queue depth). Starting sandboxes in-pod needs userns / privileges.
- **Ingress** — client submit + SSE subscribe.

Mostly buildable on what awaken has: the neutral `RunExecutor`, `AcpRunExecutor`,
the `DispatchQueue`/`RunIngress` port, self-contained `RunDispatch`, the
co-located pool, and `durable_ops_router`. A single cell reaches tens of
thousands of concurrent runs because only facts cross the hub.

## Deployment — Phase 2: multiple cells (only when a cell tops out)

`thread_id → cell` deterministic routing; each cell is a full Phase-1 copy with
its own local SQLite. No global single point; the ③ write load spreads with the
threads. The single-writer + SQLite elegance is preserved **inside** each cell —
"one" simply becomes "one per cell".

```
        Shard Router (stateless Deployment) — hash(thread_id) → owning cell
                 │            │            │
            ┌────▼────┐  ┌────▼────┐  ┌────▼────┐
            │ Cell A  │  │ Cell B  │  │ Cell C  │   … Cell N (add cell = add
            │ server  │  │ server  │  │ server  │        capacity, linear)
            │ +sqlite │  │ +sqlite │  │ +sqlite │
            │ +workers│  │ +workers│  │ +workers│
            └─────────┘  └─────────┘  └─────────┘
```

## Phased roadmap

- **Phase 0 — single process, co-located pool** *(current)*: dev / single node.
  `SharedHost::ensure_dispatch_pool` runs the pool in-process, SQLite local.
- **Phase 1 — single cell, workers on k8s**: split out the worker Deployment
  (HPA); server StatefulSet + PVC. Edge projection + local egress + tunable
  concurrency. Reaches tens of thousands of concurrent runs.
- **Phase 2 — multiple cells, shard-router**: only when one cell tops out, add
  `thread_id` sharding + cell membership. Each cell is a Phase-1 copy.
- **Phase 3 — swap a cell to Postgres on demand**: if a cell's SQLite tops out on
  writes (or needs in-cell HA), move that cell's store. Single-writer semantics
  make the swap smooth.

> **Strongest recommendation: do not build sharding now.** The shard-router +
> membership + rebalancing + failover is the largest, riskiest piece and does not
> exist in awaken. Run a single cell until it measurably tops out, then shard. The
> seam (`thread_id`) is enough to keep ready.

## Environment, single-writer HA, capacity, failure

### Subprocess env (the awaken way)

awaken's ACP subprocess `env_clear()`s the ambient env and installs only two
things: **① a `["PATH","HOME"]` host passthrough** (so `npx`/`node`/the CLI
resolve) and **② projected model/secret env** (from the config plane / vault). So
in deployment: **node ≥ 20 is baked into the worker image** (found via PATH, not a
shell hack); **credentials live in the vault, injected on demand**; **the sandbox
is decided by pod securityContext**. These are not runtime shell variables — they
are provisioned via image + config + vault.

### Single-writer HA (the one wart, faced head-on)

- **SQLite**: StatefulSet single replica + PVC re-attach + leader election gives
  seconds-scale takeover; in-flight runs recover from committed truth via lease
  expiry + `reconcile`.
- **Stronger HA**: move that cell to Postgres — the store provides HA and the
  server becomes stateless multi-replica (Phase 3).

### Capacity — when to shard / swap

| Constraint | Metric | Top-out → action |
|---|---|---|
| SQLite write throughput | facts/run × completion rate = commit writes/s | write latency ↑ → swap that cell to Postgres |
| Server relay bandwidth | ③ facts total bandwidth × concurrency | hub saturated → shard |
| Worker capacity | in-flight runs vs multiplex ceiling | queue depth ↑ → HPA scales workers |

Order: scale workers first (cheapest) → swap store (single-cell write bottleneck)
→ shard (hub bandwidth / global scale).

### Failure and recovery

| Failure | Behavior | Guarantee |
|---|---|---|
| worker crash | lease expires → another worker reclaims | at-least-once; idempotency / epoch dedupe |
| cell server crash | PVC re-attach + restart takeover (or Postgres multi-replica) | committed truth not lost; brief write unavailability |
| cell migration (Phase 2) | quiesce thread → hand off owner → recover from truth | single-owner-executes invariant; no split-brain |

## Task list — a local node as an ACP worker (awaken)

| State | Task | awaken landing |
|---|---|---|
| have | neutral execution port + AcpRunExecutor | `awaken-run-executor-acp` (`AgentChannelSource` opens the channel) |
| have | dispatch queue port + self-contained activation | `RunIngress` / `DispatchQueue` / `RunDispatch` |
| have | co-located pool + HTTP ops surface | `SharedHost::ensure_dispatch_pool`, `durable_ops_router` |
| have | single-writer commit, ACP subprocess launch | commit coordinator, `subprocess.rs` (env_clear + passthrough) |
| **done · read side** | **server→client live streaming across all three frontends** — managed live previews (`event_start`/`event_delta`), ai-sdk/ag-ui already streaming. The in-process per-session broadcast is the **read-side prototype** of the cross-node event sink (same multiplex/fan-out shape) | `awaken-protocol-managed` (`preview.rs`, live SSE), the `run_streaming` seam |
| **design-first · step 1** | **the Fact contract** — a neutral published-language type carrying identity `(thread_id, run_id, seq)` + `AgentEvent` + its delivery contract (per-thread monotonic `seq`, at-least-once with idempotent commit keyed on `(thread_id, seq)`), defined *before* any transport | a kernel type beside `AgentEvent` |
| **build · slice 1** | **ONE thin end-to-end path**: one worker → **HTTP dispatch transport** (claim/settle over HTTP) → run → push Facts → server ingest + commit + fan-out. **Workers are db-less** — they never open sqlite; the server stays the single writer. Everything else (capability, egress policy, HA) stubbed | `DispatchQueue` HTTP impl + facts ingest on `durable_ops_router`, carrying the Fact type into the read-side broadcast |
| **build · worker-only mode** | a process that runs the execution pool **off the HTTP dispatch transport** (not the co-located in-process pool), pushing facts back — the stateless worker of the cell | reuse `SharedHost`'s pool over the transport from slice 1 |
| **build · later** | **capability as a dispatch-context attribute** — "can run ACP" is a `DispatchQueue`/node property claim/selection reads, **not** a floating service; **egress local direct-out** with in-place policy | |
| **harden** | at-least-once idempotency / fencing-token (a known gap), settle-with-facts atomicity, server single-writer HA | |
| **optional · not on the path** | **`journal_mode=WAL` + `busy_timeout`, set once on the shared db** — a within-process read/write-concurrency + robustness tweak, *not* a correctness requirement: no target opens the sqlite file from multiple processes (merged = one process/one shared db; split = db-less workers over HTTP) | at the shared connection open, benefits every `with_prefix(NS)` schema |
| **Phase 2** | shard-router + cell membership + migration/failover — build only when a cell tops out | |

## Sequencing, open design, and one recorded coupling

**Start with the Fact contract, then a thin transport slice.** The cell's worker
is stateless and **db-less** — it never opens sqlite; it claims runs over an HTTP
dispatch transport and pushes facts back, and the server stays the single writer.
So there is **no "make sqlite multi-process safe" step**: neither target opens the
file from multiple processes (merged = one process over one shared db; split =
db-less workers over HTTP). WAL is an optional shared-db tweak, not a gate (see
*Storage* below). Three disciplines keep the cross-node work simple and DDD-clean:

**Pin the Fact contract before the transport.** The cross-node write path moves
exactly one thing: an edge-projected fact. Model it as **published language**, not
a generic message — a neutral type carrying its identity `(thread_id, run_id,
seq)`, its `AgentEvent` payload, and its delivery contract (per-thread monotonic
`seq`; at-least-once with an idempotent commit keyed on `(thread_id, seq)` so a
redelivery is a no-op). The HTTP dispatch transport, the multiplexed event sink,
and the server-side ingest are all *adapters* over this one type. Defining it
first stops the sink degrading into an untyped message bus and gives the
already-built read-side broadcast a typed thing to carry.

**Give capability a home — don't let it float.** "A node can run ACP" is an
**attribute of the dispatch context** (a `DispatchQueue`/node property that
claim/selection reads), not a new cross-cutting service. Fold it there, or it
becomes an anemic global.

**Build ONE thin slice before the matrix.** One worker, one transport, one Fact
type, end-to-end (claim → run → push facts → commit → fan-out), everything else
stubbed. Let capability, egress policy, fencing, and HA emerge from that slice's
real constraints rather than being committed up front — the cross-node bucket is
otherwise a big-bang list, which is the opposite of simple design.

### Storage — one shared db, prefix-isolated schemas

The merged (standalone / single-node) deployment runs all services in **one
process over one shared sqlite database**, with each service's tables namespaced
by the scoped-migration `with_prefix(NS)` bundle — `runtime_*` for the dispatch
queue, `managed_*` for sessions, and so on. (Today the bin still opens separate
`.db` files; the per-bundle prefix is exactly what makes consolidating them into
one file mechanical — the prefix is redundant only while the files are split.)

Two consequences:

- **WAL/busy_timeout is a database-level, set-once concern**, applied at the one
  shared-connection open — not a per-crate change and not on the critical path. It
  buys within-process read/write concurrency (readers don't block the single
  writer on a now-busier shared db) plus robustness against any external opener; it
  is not required for correctness because nothing opens the file from multiple
  processes.
- **This is deliberate Shared-Database integration, kept honest by the prefixes.**
  Each bounded context owns its `NS` prefix, with no cross-prefix foreign keys, so
  "one physical db, N logically-isolated schemas". When a context needs a
  multi-writer or HA store, it **extracts cleanly along its prefix into its own
  Postgres** — which is exactly the Phase-3 "swap a cell's store" story. The shared
  db is a pragmatic co-location, not a tangle.

### Recorded coupling (read side)

The shipped managed previews reuse one `evt_N` id across a boundary: the streaming
`PreviewSink` mints an `agent.message` id from the shared event-id counter and
`append_turn` reuses it, so `event_start.event.id` equals the committed
`agent.message.id` (the SDK reconciles preview → buffered by id). This is a
pragmatic, best-effort coupling — infrastructure (streaming) allocating a domain
identity — justified by the SDK's reconcile-by-id contract. **Keep it contained to
the preview path; do not generalize id minting into the streaming layer.**
