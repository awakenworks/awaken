# ADR-0001: Documentation Model And Vocabulary Alignment

- Status: Accepted
- Depends on: none
- Supersedes: the heavyweight meta-process model (readiness / document-class /
  role-catalog-coverage tables, standalone coverage matrix) in `README.md` and
  `STATUS.md`; refines the `docs/wiki/` retrieval layer to link-only (see D5)

## Context

This repository is the self-contained design home for `awaken-runtime` and the
packages it names. A separate mature **reference implementation** (the `goal`
worktree) demonstrates a lean, decision-first documentation model worth adopting
— consulted for the *pattern*, never imported as authority:

- one decision = one ADR, append-mostly, with explicit supersede/amend chains;
- a thin guardrail index that links each guardrail to its **owning ADR**, its
  **enforcer code symbol**, and its **validation test path**;
- no process/status/PM documents, no second source of truth, and no inline
  duplication of schemas, state machines, or rationale outside the canonical
  owner.

The design corpus in this repository drifted away from that model. It grew a
parallel guardrail numbering (`G1..G29`), heavyweight meta-documents (readiness
tables, document-class classification, role-catalog coverage, a requirements
coverage matrix), inline state-machine/role catalogs, and a `docs/wiki/` facts
layer that broke its own link-only rule and copied content (so it drifted from
the source). Two concrete failures followed:

1. **Internal vocabulary drift.** The corpus invented a `BackgroundTask`
   umbrella for work it already models as `ScheduledAction`, conflated its own
   distinct resolver roles (`AgentResolver` / `Resolver` / `RunResolver`), and
   used two names for the tool port (`ToolExecutionLocus` vs `ToolExecutor`).
2. **Guardrails without enforcers.** `G1..G29` named only generic check
   categories ("dependency checks", "API surface tests"), not a corpus role or
   test kind, so no guardrail was traceable.

This corpus is **self-contained**: it is the design home for its own crates
(`awaken-runtime`, `awaken-runtime-contract`, `awaken-agent-contract`,
`awaken-ext-builtin-tools`, and the further packages it names). A separate mature
implementation (the `goal` worktree) is consulted as a **reference for patterns**
— decision-first docs, enforcer traceability, simple design — but its specific
crate paths, type symbols, and ADR numbers are never imported here as authority.

## Decision

### D1: Architecture truth lives in ADRs plus code, not in a corpus

New load-bearing decisions are recorded as numbered ADRs under `docs/adr/`,
following the reference implementation's ADR style: Status / Context / Decision
(D1, D2, …) / Consequences, with explicit `Supersedes` / `Amends` headers.
Accepted ADRs are append-mostly; a meaning change requires a superseding ADR or a
dated amendment note that links the superseding ADR. Theme/overview documents may
remain as navigation. A theme document may own a role catalog only when it is the
owning design for those stable roles; otherwise it links to the owner. State
machines live in the code/Rustdoc once implemented, or in the owning design doc
until then. Navigation, status, coverage, and wiki documents must not duplicate
schemas, state machines, or role catalogs. Enforced by `scripts/ci/check_adr.py`
(title/number, `Status`, the required `## Context` / `## Decision` /
`## Consequences` sections, unique numbers).

### D2: Every guardrail names enforcer and validation

`INVARIANTS.md` is a guardrail **index**, not a rule essay. Each guardrail row
names: the statement, the **enforcer** (a concrete code symbol — type, function,
trait, or hook), and the **validation** (a concrete test path). A guardrail with
no enforcer yet is marked `Target`; it must not be presented as enforced.
Enforcer/Validation name this corpus's own roles and intended test kinds — never
another repository's paths. Enforced by `scripts/ci/check_invariants.py`, which
requires every guardrail row to carry a non-empty Statement, Enforcer, and
Validation cell (and forbids any parallel guardrail prefix).

### D3: Vocabulary is internally consistent (ubiquitous language)

Design documents use one name per concept, drawn from this corpus's own
established vocabulary. The mapping below resolves the internal drift. Where a
document needs a concept not yet named, it says so explicitly rather than invent
a competing name.

| Drifted doc vocabulary | Resolution (this corpus) | Owner |
|---|---|---|
| `BackgroundTask*` (invented type + 12th "axis" + state machine) | retired; deferred work is `ScheduledAction` (and the external wait/resume and dispatch axes) | [ADR-0003](0003-deferred-work-mechanism-selection.md) |
| `AgentResolver` / `RunResolver` conflated; "the resolver" ambiguous | three distinct roles, named explicitly | [ADR-0002](0002-resolver-role-demarcation.md) |
| `ToolExecutionLocus` vs `ToolExecutor` as the tool port | `ToolExecutor` is the sole neutral port; `ToolExecutionLocus` is removed — the executing side implements the port, and where execution runs is not a runtime role | [tool-and-capability.md](../design/tool-and-capability.md) |
| `ResolvedRun` crosses the server seam | `ResolvedRun` is runtime-internal; `ResolvedSpec` + `CatalogFingerprint` are the crossing values | [key-design-decisions.md](../design/key-design-decisions.md) D3, G3 |

### D4: One guardrail namespace, with self-contained enforcers

The corpus uses a single `Gn` guardrail namespace (enforced by
`scripts/ci/check_invariants.py` — no parallel prefix). Each `Gn` row names a
corpus role or mechanism as its enforcer and a test kind as its validation
(`Target` where not yet designed). The `Gn` series also covers broader
design-corpus concerns (packaging, licensing, vocabulary) enforced by the
`scripts/ci/` hooks rather than as runtime guardrails.

### D6: One decision-record home going forward

`key-design-decisions.md` (`D1..D20`) is the pre-ADR decision log and stays
authoritative for the decisions it records. New load-bearing decisions are ADRs
under `docs/adr/` (this is ADR-0001 onward). The two do not overlap in scope; a
later decision that changes a `Dn` is recorded as an ADR that names the `Dn` it
supersedes. There is exactly one home for any given decision.

### D5: Keep the retrieval layer (tightened), trim the meta-process layer

These layers are different kinds of document and are treated differently:

- **Retained: `docs/wiki/*` (LLM retrieval index).** A distinct, downstream layer
  whose job is fast owner-location for humans and agents — not a competing source
  of truth (its own `AGENTS.md` already states this). It stays, with discipline
  tightened and mechanically enforced: every fact is **link-only** — one sentence
  plus an owner link — and copies no schema, guardrail range, state machine, or
  table (`check-wiki-no-invariant-copy`). The retrieval layer may instead be
  collapsed into a single thin index (the reference implementation does this in
  its `CLAUDE.md`); either form is acceptable, but a drifting *parallel* facts
  corpus is not.
- **Trimmed: the meta-process surface.** `STATUS.md` readiness table,
  document-class classification, and role-catalog coverage, plus
  `requirements-coverage.md` as a standalone matrix, are process/status
  documents. They are slated for simplification once their load-bearing facts
  fold into ADRs or the guardrail index.

Any removal is a separate, explicit change, not silent deletion.

## Consequences

- The guardrail index becomes mechanically traceable: reviewers follow
  guardrail → enforcer symbol → test.
- Documents stop drifting from code because they name real types and link to
  canonical owners instead of restating them.
- The repository sheds its meta-process surface; new decisions cost one ADR plus
  the owning design/code update and, optionally, a one-line wiki link. Role
  catalogs are maintained only by their owning design docs until Rustdoc/API
  becomes the better owner.
- One-time cost: reconciling existing documents to the vocabulary in D3 and
  trimming the D5 meta-process tables.

## Amendment (2026-06-30): the bar for a new ADR

D1 says new load-bearing decisions are ADRs but never defined *load-bearing*, so
the corpus grew one ADR per delivered slice — the dispatch subsystem alone took
~17 (0009–0027). This tightens D1: an ADR is for a **contested decision**, not a
feature record.

A new ADR is warranted only when all three hold:

1. **A rejected alternative.** Two or more viable approaches existed and one was
   chosen over the others for a stated reason. One obvious option → no decision.
2. **Re-litigation risk.** A future reader would otherwise reopen the question.
   If a code comment or design doc settles it, it is not an ADR.
3. **Reach.** It crosses crates or changes a `Gn` invariant. A local
   implementation detail is not an ADR.

Otherwise the change is a **dated amendment to the ADR it refines** (per D1's
append-mostly rule), or just a commit plus the owning design/code update. A
refinement of a prior ADR's *deferred* item is always an amendment to that ADR,
never a new number — keeping one decision in one home (D6). Self-check: if a
draft's `## Context` cites something a previous ADR "deferred" or "named", it is
that ADR's amendment. This bar is judgment, not mechanizable; `check_adr.py`
checks structure only.

## References

- `INVARIANTS.md` — guardrail index with enforcers (D2).
- [key-design-decisions.md](../design/key-design-decisions.md) — the pre-ADR
  decision log (D6).
- The `goal` worktree is a reference for the decision-first pattern only; its
  ADR numbers and crate paths are not authoritative here (Context).
