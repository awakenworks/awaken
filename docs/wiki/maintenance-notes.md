---
type: Maintenance Notes
title: Wiki Maintenance Notes
description: Durable maintenance policy for the OKF-compatible retrieval wiki.
tags: [wiki, okf, documentation]
timestamp: 2026-06-27T00:00:00+08:00
---

# Wiki Maintenance Notes

The wiki is a retrieval index over source design documents. It should stay
compact: facts point to owners, owners carry the full design.

## Current Scope

- Source documents remain authoritative; wiki pages carry retrieval facts only.
- The current guardrail set is owned exclusively by
  [INVARIANTS.md](../INVARIANTS.md); downstream wiki pages do not copy its range.
- The wiki indexes the config-to-run flow, runtime interface boundaries, runtime
  behavior, tool/capability policy, deployment boundaries, resources, credentials,
  and downstream product adapters.
- `index.md` and `log.md` are OKF reserved files and therefore do not carry
  frontmatter.
- `log.md` is only the OKF update history. Durable maintenance policy belongs in
  this file.
- Do not add per-edit process history unless it records a durable maintenance
  policy that future contributors must follow.
