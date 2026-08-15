# Session Branch Prefixes

## End-to-End Objective

A product may create a new Managed Session whose model starts with an immutable
prefix of another Session, without copying source events or messages into the
target Thread. The source committed Thread remains the only history authority;
the target Session persists only a `TranscriptSliceSpec` and owns only its new
turns.

This design supplies the protocol-neutral compatibility mechanism. Product
selection, labels, branch trees, and navigation remain application concerns.

## Reuse and Change Classification

| Classification | Authority | Role |
|---|---|---|
| Reused unchanged | `CommittedThreadView`, `TranscriptSnapshotRef`, `TranscriptSliceSpec` | Freeze, verify, and reconstruct one append-only prefix |
| Reused unchanged | `RuntimeRunContext.request_context` | Make derived messages model-visible without committing them |
| Existing mechanism modified | Session baseline and frozen Worker projection | Persist the slice reference; carry only its rebuildable materialization across placement |
| Existing mechanism modified | None | No public ingress currently authors this internal baseline field |
| Genuinely new public mechanism (deferred) | Typed native control-plane Session command | A future product API may admit a transcript prefix after Workspace and range validation; Managed Session create has no private ingress |
| Genuinely new durable aggregate/store | None | No branch tree, transcript copy, event copy, or context cache is durable truth |

## Static Structure

```text
source Thread committed messages
          |
          v
TranscriptSliceSpec ---------> target SessionBaseline
                                      |
                                      v
SessionApplication projection --materialize--> FrozenSessionProjection.request_context
                                                     |
                                                     v
RuntimeRunContext.request_context + target committed Thread + current input
```

The baseline reference is fingerprinted with the target Session's other frozen
execution inputs. `FrozenSessionProjection.request_context` is a transport and
process cache only. It is reconstructed from the reference for local recovery
and before remote Worker realization.

## Dynamic Behavior

```text
future typed native command supplies source Session id + optional end_seq
  -> authorize source in target Workspace
  -> read committed source messages
  -> reject end_seq beyond committed end
  -> freeze RawCommitted[0,end_seq)
  -> persist target Session baseline (target Thread still empty)
  -> realization reconstructs and verifies the frozen slice
  -> install slice as request-only context
  -> ordinary Run sees prefix + target history + current input
  -> commit only target input/output deltas
```

Omitting `end_seq` freezes the source's latest committed end at admission.
Concurrent source appends cannot change the stored version or selected range.
Recovery reads committed Thread truth directly through the trusted persisted
reference; a source Session projection may be archived or deleted without
changing its retained committed transcript.

## Cause/Effect Coverage

| Rule | Causes | Effects | Executable owner |
|---|---|---|---|
| B1 | same-Workspace source, valid end | exact frozen reference; exact request context; empty target transcript | required with the deferred typed command |
| B2 | end beyond committed source | reject before target creation | required with the deferred typed command |
| B3 | missing/cross-Workspace source | existing Session ownership guard rejects | required with the deferred typed command plus ownership guard suite |
| B4 | local or remote frozen projection | context precedes current input; source ids never commit | `control_frozen_baseline_is_the_only_worker_runtime_projection` |
| B5 | fresh or resumed Runtime attempt | common model-transcript assembly consumes request context | `multi_run::request_context_is_model_visible_but_never_committed` |

## Required Invariants

1. A target stores no copied source event or message.
2. A prefix is immutable after target creation, even if the source advances.
3. Workspace authorization happens before target identity or durable mutation.
4. Materialized prefix messages are never passed to `ThreadCommit.new_messages`.
5. Local and remote realization consume the same frozen projection.
6. Product branch metadata is descriptive and cannot select transcript truth.
7. Managed Session requests cannot author a transcript prefix; unknown private
   fields fail before persistence.
