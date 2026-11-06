# Changelog

Notable changes to `awaken-runtime-contract`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- The forwarding-only `credential` module and the first-party Agent/Tool forwarding paths are removed; import those items from `awaken-agent-contract` and `awaken-tool-contract`.
- `diagnostic::Error` gains the variant `Admission`.
- `llm::{ThreadUsage, ThreadUsageKey, THREAD_USAGE_STATE_KEY}` are removed; import them from `awaken_agent_contract::thread::usage`.
- `resilience::RetryPolicy::decide` takes the `ModelReentry` projection instead of `ModelExecutionEvidence`.
- `live_inbox::LiveInbox::version` returns `Option<u64>`, and `EditError` and `WaitOutcome` gain `Unavailable`: a poisoned inbox admits nothing, reads as closed and empty, and reports itself unavailable instead of panicking.
- Its public API names types of `awaken-agent-contract` 2.0 and `awaken-tool-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.
- `RuntimeRunContext` gains the public field `cancellation_cause`: who fired the attempt's cancellation token. Only a cause that ends the Run lets an executor commit `Ended(Cancelled)`; an attempt stopped by authority loss (proof expiry, lost claim, Worker revocation, host drain) commits nothing and returns the `attempt_abandoned` diagnostic.

### Added

- `cancellation` module: `CancellationCause`, `CancellationCauseCell`, `AttemptCancellation`, `AbandonedAttempt`, `abandoned_attempt`, `abandoned_attempt_cause` and `ABANDONED_ATTEMPT_CODE`.
- `RuntimeRunContext::{with_cancellation_cause, abandoned_by, admit_terminal_commit}`.
