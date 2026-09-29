# ADR-0085: ACP Instructions and Session Time Context

- Status: Accepted
- Date: 2026-09-29
- Amends: [ADR-0066](0066-session-service-binding-and-realization.md),
  [ADR-0069](0069-acp-capability-configuration-lifecycle.md)

## Context

ACP has no portable system-instruction field. Placing every Agent instruction
in the first User prompt weakens its role and repeats it after a real system
channel is available. Native also needs the same task date and timezone as ACP;
a vendor CLI's process date cannot define the Session's business timezone.

## Decision

The existing ACP catalog declares supported instruction channels. The existing
`project_launch` resolves model, environment, argv, tool aliases and instructions
once; plain and Session-bound sources consume that projection. Session-bound
sources materialize required files through their existing Environment. No second
catalog, prompt service, credential owner or launch pipeline exists.

`instructions.rs` selects replacement when aliases disable builtins, otherwise
append. Claude uses top-level `_meta.systemPrompt`; Codex merges its existing
JSON config; OpenCode references a Session-owned file; Hermes uses its environment
hint; Gemini uses its managed global-memory file. Unusable file routes report
FirstPrompt with an explicit reason. Gemini backend-owned home and import syntax
are explicit exceptions. Unsupported replacement fails before process launch.
Claude Clear uses a truthy newline transport because its pinned adapter ignores
an empty string; actual SDK/provider tests verify that the preset stays replaced.

Agent text has one source: the resolved frozen instructions. A system route
omits the first-prompt copy. The shared `NormalizedModelInput` still owns the
fallback envelope, task input and request context. Replacement removes the
selected preset; it does not claim to remove vendor safety/identity additions.

`SessionContextSeed` owns the versioned encoding inside `SessionBaseline.prompts`.
Session application admission validates explicit IANA timezone, defaults to UTC
and rejects the reserved prompt prefix. Deployment application projects the
committed Run trigger and cron timezone into the same create path. Public
metadata cannot declare a trusted Deployment origin. The derived launch fields
do not alter the historical authoring fingerprint and must be reconstructed from
the committed DeploymentRun when a launch is deserialized.

The seed freezes Session creation lineage and timezone, not calendar date or
authorization. Scheduled lineage derives the unattended fact; a later input
does not reauthor that lineage. Historical/no-baseline attempts use explicit
legacy_unknown and UTC. Existing baseline bytes/fingerprints are not rewritten.

## Dynamic behavior, consistency and failures

`SessionContextAttemptExecutor` wraps the complete execute/resume route for
Native, ACP and A2A. It decodes only the frozen baseline, consumes the machine
seed, samples its injected Host clock once and emits one request-only environment
message. Its id includes the seed fingerprint: reprojection reuses the sample,
and a newly installed seed replaces the previous environment projection.
Fresh attempts sample again. Publication and durable User input remain unchanged.
Native normalization retains separate request-context and transcript views.
Phase hooks and compaction receive conversational history; inference prepends
request context through the existing protected prelude. Environment facts are
neither summarized as old conversation nor hidden by the compaction window.

ACP projects the same timezone into process TZ. Task-time context remains
authoritative because a vendor may discover its timezone independently or retain
an old date in its own history. Disabling unrelated vendor filesystem/tool
context merely to remove a date is outside this decision.

New, load and missing-session fallback share the existing handshake. Each
relaunch resends session metadata, environment and required files. Historical
notifications remain discarded by the existing replay owner. Instruction
placement enriches the existing Ready event rather than adding another phase.
Malformed/duplicate seed, invalid JSON config and file materialization failures
fail closed without sending a model prompt.

Portable Session data uses `DirSessionHome` and `FsSessionBlobStore`; tests no
longer copy these mechanisms. The existing `SessionHomePlan.exclude` filters
both store and restore. OpenCode's pinned data directory is `opencode`, including
its SQLite database; credential files do not move with it. Rows without portable
subtree support retain their Session-owned home across process recovery.

## Validation boundary

Cause-effect rules live beside the instruction, handshake, seed, application
admission, Host projection and Session-home tests. The actual Claude SDK/adapter
suite observes provider system fields for replace, append, Clear and repeated
approval/load boundaries. The existing live recovery target uses the canonical
launch, commit/read and Session-home implementations and requires real model
output, retained memory, tool facts and exact file bytes.

Live executor/vendor evidence does not certify distributed placement or every
vendor's complete safety/identity behavior. A deliberately skipped live provider
is reported as unqualified, never as a passing deterministic substitute.

## Consequences

Calendar facts and instruction delivery share the existing attempt and launch
owners. Their added context has an inference cost, while task timezone no longer
depends on the selected CLI or Worker locale. Vendor-specific exceptions remain
explicit. Session data portability follows declared subtrees and exclusions;
this decision adds no credential transfer, authorization grant or recovery store.
