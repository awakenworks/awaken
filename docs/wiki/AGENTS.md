---
type: Agent Instructions
title: Wiki Agent Instructions
description: Rules for automated contributors editing the OKF-compatible wiki.
tags: [agents, wiki, okf, documentation]
timestamp: 2026-06-25T00:00:00+08:00
---

# AGENTS.md

Guidance for automated contributors editing `docs/wiki/`.

## Purpose

The wiki is Open Knowledge Format-compatible. It contains short repo-local owned
fact records, terminology anchors, and engineering lessons that help humans and
agents find the authoritative owner. It is a retrieval index, not a second source
of truth.

## Maintenance Contract

- `docs/design/*.md`, [INVARIANTS.md](../INVARIANTS.md), and
  [requirements-coverage.md](../requirements-coverage.md) are the source
  documents.
- This wiki is downstream. Update wiki facts when source documents evolve; do
  not edit source documents just to match wiki wording.
- On conflict, the source document wins. Correct the wiki fact or flag the
  divergence for review.
- Keep facts short and link-heavy. Schemas, matrices, endpoint lists, and
  invariant statements belong in the source documents.

## Rules

- Every ordinary concept Markdown file in this directory must have YAML
  frontmatter with a non-empty `type` field.
- `index.md` and `log.md` are OKF reserved files. They must not carry
  frontmatter; `index.md` is navigation and `log.md` is update history.
- Every fact has one owner: a design document, [INVARIANTS.md](../INVARIANTS.md),
  or [requirements-coverage.md](../requirements-coverage.md).
- The owner is never the wiki page itself.
- Use the current architecture guardrail ids (`G1` through `G32`) only.
- Use relative Markdown links for repo-local targets.
- Lessons state durable engineering rules, not investigation notes.
- Do not add per-edit process history. Use [maintenance-notes.md](maintenance-notes.md)
  only for durable maintenance policy future contributors need.
- Do not mention real provider credentials, operational secrets, or local
  reference-project paths.

## Format

Use IDs like `FACT-WAIST-001` or `FACT-LESSON-001` and include:

```text
- Status:
- Owner:
- Fact:
- Links:
- Verification:
```

When a new `docs/design/*.md` document is added, give it an ownership row in
[document-ownership.md](document-ownership.md) and add a fact page only if it
needs retrieval anchors.
