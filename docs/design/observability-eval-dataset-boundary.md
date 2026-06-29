# Observability, Eval, And Dataset Boundary

This document separates diagnostics, analytics, datasets, eval, and experiments
from runtime truth. These systems are important consumers of runtime facts, but
they must not become alternate commit paths.

## Data Classes

| Data class | Owner | Source | Runtime authority |
|---|---|---|---|
| trace span | observability adapter | runtime events, provider/tool timing, diagnostics | none; diagnostic only |
| metric | analytics sink | committed facts or live counters | none; aggregate only |
| dataset row | dataset builder | committed messages/events/facts plus redaction | none; training/eval input only |
| eval run | eval harness | runtime ports with controlled config/provider adapters | same as normal runtime execution |
| judge result | eval system | committed output and judge adapter | projection; not runtime truth |
| experiment route | config/product policy | pre-activation routing decision | selected config input only |

Runtime facts and commits are the source. Observability and eval outputs may
explain, score, or compare runs, but they do not rewrite run truth.

## Trace Rules

1. Trace ids may cross boundaries as correlation metadata.
2. Trace spans are not commit records.
3. A missing trace must not change runtime behavior.
4. Secret, credential, and raw resource payload redaction happens before analytics
   export.
5. Public protocol ids are adapter projections; traces may reference them only as
   correlation tags.

## Eval Rules

Eval execution uses the same runtime ports as production execution:

```text
eval scenario
  -> configured provider/tool adapters
  -> RunActivation
  -> runtime execution and commit
  -> committed facts/events
  -> dataset/eval projection
  -> judge or report
```

Mock providers, judge models, and experiment routers are adapters. They do not
create special runtime semantics.

## Dataset Rules

Dataset capture must name:

- source committed facts/events/messages;
- redaction policy;
- schema version;
- lineage to run/thread/config version;
- whether tool inputs/results are included;
- whether public protocol projection fields are included.

If a dataset row cannot be traced to committed runtime facts or an explicit
projection source, it is not admissible as a runtime-derived dataset.

## First Vertical Slice

1. Run one scenario through normal runtime ports.
2. Commit messages, events, and final run facts.
3. Export one trace and one dataset row from committed data.
4. Score the row with a judge adapter.
5. Prove deleting trace data does not change replayed runtime truth.

## Guardrails

G1, G10, G13, G23, G25, and G26 in [INVARIANTS](../INVARIANTS.md).
