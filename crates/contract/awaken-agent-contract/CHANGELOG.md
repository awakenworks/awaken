# Changelog

Notable changes to `awaken-agent-contract`. Versions follow the [SDK versioning policy](../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- `resilience::Disposition` gains the variant `StopRouting` (exhaustive enum).

### Added

- `thread::usage::{ThreadUsage, ThreadUsageKey, THREAD_USAGE_STATE_KEY}`: committed Thread token usage, moved here from `awaken-runtime-contract::llm`.
- `thread::background_task::{BACKGROUND_TASK_STATE_PREFIX, background_task_state_key, committed_task_slot, committed_task_attempt}`: the committed BackgroundTask slot's state key and the exact origin, holder and nonzero epoch of its current attempt, so platform crates read a Task's held coordinates without depending on `awaken-ext-background-task`.
- `credential::CredentialHandlingMode` and `CredentialExecutionPolicy::{handling_mode, application_managed}`.
- `RunRecoverySnapshot` read accessors and `message::Id` agent-input constructors.
