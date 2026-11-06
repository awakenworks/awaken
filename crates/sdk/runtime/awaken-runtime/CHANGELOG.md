# Changelog

Notable changes to `awaken-runtime`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- `EmbeddedSession` and `Runtime::session` are removed; use `Conversation` and `Runtime::conversation`.
- `Runtime::with_max_consecutive_inference_failures` is removed.
- Its public API names types of `awaken-agent-contract` 2.0, `awaken-tool-contract` 2.0 and `awaken-runtime-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.
- `Runtime::registered_attempt_cancellation` returns `Option<AttemptCancellation>` (the token with its cause cell) instead of `Option<CancellationToken>`.
- A Run whose attempt was cancelled for a cause that does not end it (`ProofExpired`, `ClaimLost`, `WorkerRevoked`, `HostDrain`) is no longer committed `Ended(Cancelled)`: the attempt returns the `attempt_abandoned` diagnostic and writes no terminal fact. An untagged or `Requested` cancellation still ends the Run `Cancelled`.

### Added

- `validate_configuration`.
