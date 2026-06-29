---
type: Directory Guide
title: Wiki
description: Guide for the OKF-compatible LLM wiki of the Awaken runtime design corpus.
tags: [wiki, okf, documentation, agent-context]
timestamp: 2026-06-25T00:00:00+08:00
---

# Wiki

This wiki is an Open Knowledge Format-compatible documentation area for the
Awaken runtime design corpus. It exists so humans and agents can find compact
facts and then jump to the authoritative design document.

The source documents are [design](../design), [INVARIANTS.md](../INVARIANTS.md),
and [requirements-coverage.md](../requirements-coverage.md). This wiki is
downstream of those documents. On conflict, update the wiki; do not treat a wiki
fact as the source of truth.

Within this repo, individual wiki entries are owned fact records:

```text
OwnedFact {
  id,
  status,
  owner,
  fact,
  links,
  verification
}
```

## Rules

- One fact has one owner.
- The owner is a local source document, not the wiki page itself.
- Facts are short and reviewable. Link to source tables and guardrails instead
  of copying them.
- Use current guardrail ids (`G1` through `G29`) from
  [INVARIANTS.md](../INVARIANTS.md).
- Use relative Markdown links inside the repo.

## Index

- [Wiki index](index.md)
- [Document ownership](document-ownership.md)
- [Engineering lessons](engineering-lessons.md)
- [Wiki maintenance notes](maintenance-notes.md)
- [Wiki update log](log.md)

## Fact Pages

- [Neutral waist facts](neutral-waist-facts.md)
- [Config to run execution flow facts](config-to-run-execution-flow-facts.md)
- [Runtime interface boundary facts](runtime-interface-boundaries-facts.md)
- [Runtime behavior facts](runtime-behavior-facts.md)
- [Tool and capability facts](tool-and-capability-facts.md)
- [Run ingress and message delivery facts](run-ingress-message-delivery-facts.md)
- [Product protocol and session facts](anthropic-alignment-and-sessions-facts.md)
- [Credentials and vaults facts](credentials-and-vaults-facts.md)
- [Resources, memory, files, and skills facts](resources-memory-files-skills-facts.md)
- [Wiki maintenance notes](maintenance-notes.md)

## Template

```text
## FACT-AREA-001: Short fact title

- Status: active | proposed | retired
- Owner: relative link to the owning source document
- Fact: one durable statement
- Links: related docs and guardrail ids
- Verification: test, review, CI check, or guardrail enforcer
```
