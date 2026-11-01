# Changelog

Notable changes to `awaken-tool-contract`. Versions follow the [SDK versioning policy](../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- `tool_batch::ToolCallPhase` gains `KnownAborted` and `Returned`.
- `ActiveToolBatch::{operation_for_message, recorded_batches}` are removed.
- Its public API names types of `awaken-agent-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.

### Added

- `tool::background_task` is the single Task observation codec and pure identity/tenure/wait/end vocabulary, consumed directly by the extension and authenticated Task admission. It reuses `awaken-lease-contract` for its original exact fence.

- `tool_configuration` module: `ToolConfigurationCompiler`, `ToolConfigurationInput`, `ToolConfigurationError`, `compile_toolset_configurations`.
- `tool_batch::ToolExecutionDomainFootprint`.
