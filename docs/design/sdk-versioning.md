# SDK versioning

Status: policy in force, enforced by `scripts/ci/check_sdk_semver.py`; the
pending `2.0.0` cohort is not yet published.
Decision record for plan v5 item L. ADR-0086 owns the SDK surface; this note
owns how that surface is versioned.

## Which crates are the SDK

Every crate whose `[package.metadata.awaken]` says `product = "sdk"` and that
Cargo may publish. `scripts/ci/public_api_selection.py` makes the same
selection for the snapshot gate and refuses an SDK crate without
`api_snapshot = true`. Today: 24 crates. `sdk-standalone`
(`_bounded_context_fitness.py`) keeps them free of Platform crates.

## Baseline

`[workspace.metadata.awaken] sdk_semver_baseline` in the root `Cargo.toml`
names the last SDK release: its `public-api/<crate>.txt` and crate versions are
what downstream code compiles against. It is
`50b3f3e22dbfe1d91dd6ee6fef8ee8dfc8e15940`, the first pre-release source
contract of ADR-0086, because no SDK release has been published since. When an
SDK release is published, the baseline moves to that release commit in the same
change.

Versions are relative to the baseline, not to the previous commit. A crate
already carrying a major bump over the baseline absorbs further breaking
changes until it is released; a second major is never stacked on an
unpublished one.

## From snapshot diff to bump

| Snapshot change against the baseline | Required bump |
|---|---|
| none | none (a patch bump is allowed) |
| new item: module, type, function, inherent method, impl | minor (`0.y`: patch) |
| variant added to a `#[non_exhaustive]` enum, field added to a `#[non_exhaustive]` struct | minor |
| any line removed or changed (signature, path, bound) | major (`0.y`: minor) |
| variant added to an exhaustive enum, field added to an exhaustive struct or an existing variant | major |
| item added to an existing trait | major |
| `Drop` added to a type with public fields | major |
| public API names an item of another SDK crate whose version is a major bump | major |

The last row is public dependency identity: a `1.x` type and a `2.x` type are
different types, so a crate that exposes one must move with it (ADR-0086,
2026-10-20).

The snapshot text cannot see private fields, provided-versus-required trait
methods or auto traits (`--omit auto-trait-impls`), so the table reads it
conservatively. The release qualifier (`check_sdk_consumer.py
--qualify-semver`, pinned cargo-semver-checks) stays the authority for what
the text cannot show; this check is the per-commit floor.

A version whose number moved needs a `## [<version>]` section in the crate's
`CHANGELOG.md`, listing its Breaking and Added items. A crate with no baseline
snapshot is new and starts at its declared version.

## Gates

| Gate | When | Proves |
|---|---|---|
| `check_public_api.sh` | pre-push (affected crates), full gate | the committed snapshot is the compiled surface |
| `check_sdk_semver.py` | pre-commit, full gate | each SDK crate's version and changelog cover its snapshot change |
| `check_sdk_consumer.py --qualify-semver` | release | cargo-semver-checks findings match the declared cohort |

A hand-edited snapshot (when a lane may not compile) is marked in its commit
and in the batch report; the integrator's `check_public_api.sh --bless` run
replaces it with the rendered text.

## Version table at this base

| Crate | Baseline | Now | Why |
|---|---|---|---|
| agent-contract, runtime-contract, tool-contract, runtime, provider-genai, ext-builtin-tools, ext-goal, ext-memory | 1.0.0 | 2.0.0 | own breaking changes (each crate's `CHANGELOG.md`) |
| ext-background-task, ext-compact, ext-permission, observability, store-inmem | 1.0.0 | 2.0.0 | expose Agent/Tool/Runtime contract 2.0 types |
| ext-mcp, ext-skills, ext-state-machine | 0.1.0 | 0.2.0 | same, pre-1.0 |
| web-policy-contract, outbound-http | 1.0.0 | 1.1.0 | additions only |
| mcp-wire | 0.1.0 | 0.2.0 | `McpTransportError` gains a variant for the classified transport fault (ADR-0103) |
| lease-contract, time, tool-pattern | unchanged | unchanged | no surface change |
| sync, process-win32 | - | 1.0.0 | new SDK crates |

## Decisions on the changes accumulated for L

| Change | Decision |
|---|---|
| `awaken-ext-goal`: `run_settlement::{RunSettlement, OutcomeRuns}`, `Controller::new(&dyn OutcomeRuns)` | breaking; inside the pending `2.0.0`, recorded in its changelog |
| `LiveInbox` lock | `InvariantLock`. A poisoned inbox admits nothing (`Offer::Closed`, wake no-op), reads as closed and empty, and reports `version() -> Option<u64>` `None`, `EditError::Unavailable`, `WaitOutcome::Unavailable`. Session execution maps both to `LiveInboxError::Unavailable`. Breaking, inside runtime-contract `2.0.0` |
| runtime-contract `llm::{ThreadUsage, ThreadUsageKey, THREAD_USAGE_STATE_KEY}` | removed: they forward `awaken-agent-contract::thread::usage`, and runtime-contract is in a pending major (ADR-0088 2026-10-28 deferred them to this release). Callers import the Agent owner. awaken-cloud uses none of them; awaken-flow (pinned at `f6ccb85caa`) imports `llm::ThreadUsage` once and changes that import when it moves its pin |
| 15 SDK optional-role sites (class (iv) of [injection cleanup](injection-and-error-cleanup.md)) | kept; reasons below |

### Optional roles in the SDK API

| Sites | Shape | Why it stays |
|---|---|---|
| `RawTool::durable_tasks`, `ToolExecutor::durable_tasks` (tool-contract ×2 declarations, ×1 registry impl), ext-mcp `tool.rs`, runtime `tool_execution.rs`; `RunDelegationService::cancellation` (runtime-contract) and its Platform impl in session-executor `delegate.rs` | `fn capability(&self) -> Option<&dyn Capability>` on an open SDK trait | the ADR-0080 capability ladder: the same idiom as `std::error::Error::source`. Third-party implementers make the set open, so a closed enum cannot name the cases, and a required supertrait would make every tool fake polling and cancel (P5 interface segregation). It is a query of the implementer, not a role injected into a component |
| `RunPublicationClosure::freeze`, `resolve_delegate_snapshot`, `freeze_delegation_publications` (runtime-contract) | `source: Option<&dyn PublishedAgentSnapshotSource>` on a pure function | `None` is the embedded SDK host without an authoring plane: delegates resolve to `SnapshotNotFound`, optional auxiliaries are skipped, custom ones refused. Platform callers hold the source as a required constructor role (class (iii)) and pass `Some` |
| `verify_attempt_ownership` (runtime-contract) and its two forwarding calls in runtime `inference.rs` | `ownership: Option<&dyn AttemptOwnershipVerifier>` | `None` is the unclaimed direct topology, the `AttemptOwnershipAdmission::Unbound` row of its Kani-proved decision table; the value comes from `RuntimeRunContext`, which a deployment selects once |
| `RunToolCapability::effective`, `ToolCapabilityNarrowing::evaluate_run_policy` (runtime-contract) | `policy: Option<&dyn ToolPermissionPolicy>` | `None` is "no Run policy": the configured ceiling, and no ruling, so the executor's own authorization still decides |

Each is a pure function argument or a capability query whose `None` has one
documented meaning. Changing them would rename that meaning without removing a
late binding, so the ratchet keeps them as reviewed SDK shapes.
