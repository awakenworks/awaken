# Auxiliary Context Windows

## End-to-End Objective

Memory Extraction, Dream Memory Consolidation, Compact, and Outcome evaluation
reuse one immutable transcript service and one ordinary Agent Run lifecycle.
They differ in the evidence they select, the result they own, and whether the
parent Run may continue before the auxiliary result is ready.
Committed Thread history remains the only conversational truth; caches and
background tasks are replaceable accelerators.

```text
committed Thread
      |
      v
TranscriptSnapshot (thread, view, version, message ids)
      |
      +-- Memory Extraction Window --> durable intent --> stable Extractor Run
      +-- Memory Recall Window -----> request-only context
      +-- Compact Fold Window ------> stable Compactor Run --> summary + bridge
      +-- Goal Evaluation Window ---> stable Grader Run
      +-- Dream Session Windows ----> JSONL files --> Memory Consolidator Run
      `-- Main Inference Window ----> model request
```

The bounded focus of this design is window selection and auxiliary execution.
MemoryStore content, Outcome state, provider prompt caches, and protocol
projection retain their existing owners.

## Static Structure

| Component | Owns | Depends on | Contract |
|---|---|---|---|
| `ThreadReader` | committed read model | Thread store | freeze a versioned `TranscriptSnapshot` |
| `TranscriptSliceSpec` | range selection | snapshot only | validate and materialize explicit ranges |
| Memory Recall plugin | query and bounded injection | current `RunInput`, MemoryStore | request-only messages; never commits recalled text |
| Memory Extraction controller | intent, retry, receipt | terminal snapshot, stable Run, MemoryStore | at-least-once observation, exactly-once effect |
| Compact plugin/backend | thresholds, fold range, artifact cache | snapshot, stable Run | soft prefetch; hard join; no raw-history rewrite |
| Outcome controller/Grader | evaluation range and state transition | snapshot, stable Run, Thread state | frozen evidence range; version-guarded transition |
| Dream Memory Consolidation | explicit cross-Session Memory curation | Session snapshots, JSONL exports, source/result MemoryStores, ordinary Managed Session | product-owned lifecycle and mounts defined by [Managed Dream Memory Consolidation](managed-dream-memory-consolidation.md) |
| Runtime Host | concrete Run/store/provider wiring | neutral ports above | no Memory/Compact/Outcome lifecycle ownership |
| ACP/A2A adapters | external execution/projection | prepared input and ordinary Run facts | no extension vocabulary or independent memory truth |

An auxiliary Thread needs no business-valued type field. Its stable Thread and
Run ids encode correlation, while its pinned Agent snapshot defines capability.
The store may attach operational metadata for observability, but correctness
must not branch on labels such as `memory` or `compact`.

The per-extension indexes have deliberately different durability:

- Memory extraction intent/receipt is durable because it controls a mutable
  MemoryStore effect.
- Outcome head and evaluation records are durable Thread state because they
  control workflow continuation.
- Compact artifacts are an in-process cache. The stable Compactor Run is the
  recoverable truth; a cache miss recomputes or joins that Run.
- Dream is a durable product job because it creates an independently governed
  result MemoryStore. Its Session snapshots and JSONL artifacts are immutable
  evidence; its ordinary Agent Run is execution truth, not Dream state truth.
- Transcript snapshots are version-addressed immutable read values reconstructed
  from committed history. Their materialized messages may be cached, but the
  cache is not a second transcript store.

## Dynamic Behavior

### Main Run and Memory

```text
submit RunInput
  -> freeze current committed prefix
  -> BeforeInference recall(current RunInput)
  -> inject selected Memory request-only
  -> main inference and terminal commit
  -> terminal observer CAS-creates extraction intent
  -> return parent result
  -> background reconciler claims intent
  -> stable Extractor Run over frozen raw ranges
  -> CAS Memory mutations
  -> durable receipt / bounded terminal failure
```

`Awaiting` is not terminal and cannot trigger extraction. Duplicate observation,
restart, or a crash between Extracted and Stored resumes the same intent. A
Memory write is accepted only once by content hash and optimistic comparison.

### Compact

```text
BeforeInference
  -> estimate current window
  -> below soft threshold: no action
  -> soft threshold: start/join stable Compactor Run; parent does not wait
  -> hard threshold:
       ready covered prefix -> reuse artifact
       in-flight exact prefix -> join it
       miss -> run stable Compactor Run and wait
  -> inject summary + verbatim bridge
  -> set dynamic ContextWindow only after coverage succeeds
```

Failure before a valid artifact leaves `KeepAll` active. Thus compaction can
increase latency at the hard boundary but cannot hide uncovered history.

### Outcome / Goal

```text
Outcome phase requests evaluation
  -> freeze RawCommitted snapshot
  -> select Worker evidence range
  -> submit stable Grader Run through ordinary backend
  -> wait (the verdict gates continuation)
  -> version-check Outcome head
  -> commit Evaluation and next transition
```

The Grader backend may be asynchronously queued and recovered, but evaluation is
not fire-and-forget: the controller must wait for the terminal verdict before it
may advance the Outcome.

### External runtimes and failure boundaries

Native, ACP, and A2A are execution axes, not storage axes. The service prepares
the same extension context and all terminal facts return through the same
commit boundary. A database-less distributed worker's `HostCommit::Remote`
remains intentionally write-only; multi-turn history on such a worker requires
a separate authenticated asynchronous read contract and is not silently
emulated from partial activation input.

Provider prompt caching is opportunistic. Reusing an identical model, system
prefix, tools, and prior messages can reduce cost, but cache hits never replace
stable Run ids, transcript snapshots, or receipts. Codex additionally requires
the OpenAI Responses API; a Chat-Completions-compatible endpoint alone is not a
Codex-compatible provider.

## Causal Graph Coverage

```text
C1 terminal Ended --------+--> E1 frozen extraction range
C2 duplicate/restart -----+--> E2 one Memory effect + terminal receipt
C3 current RunInput ------+--> E3 recall excludes stale historical query
C4 soft threshold --------+--> E4 parent inference is non-blocking
C5 hard threshold/cache --+--> E5 covered prefix + verbatim bridge, no loss
C6 Outcome evaluation ----+--> E6 frozen evidence range + guarded transition
C7 Native/ACP/A2A --------+--> E7 one Run/Thread lifecycle
C8 shared MemoryStore ----+--> E8 every runtime reads every writer
```

The executable suite is `e2e/auxiliary_windows_causal_graph_e2e.mjs`. Internal
range, phase, idempotency, and cache-join branches are covered by the owning Rust
package tests; the causal E2E suite covers their externally observable composed
effects, restart boundaries, and runtime/protocol matrices.

## Required Invariants

1. No window may include messages beyond its frozen snapshot version.
2. Recall derives its query from current `RunInput`, not the last historical User
   message.
3. Recalled Memory and Compact summaries are request-only.
4. No raw committed message is rewritten or deleted by compaction.
5. A background task is never durable truth.
6. Stable auxiliary identity plus committed facts prevents reinference after
   recovery.
7. Extension state and MemoryStore mutations cross their own consistency
   boundaries with explicit CAS; no distributed transaction is assumed.
8. Protocol and Agent runtime selection cannot create another MemoryStore truth.
