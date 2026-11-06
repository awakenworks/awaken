# Changelog

Notable changes to `awaken-ext-background-task`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- The Task observation codec, id/tenure, wait/end, completion/wait-candidate and error values move to `awaken_tool_contract::tool::background_task`; import their defining paths. The aggregate, transitions, plugin and supervisor remain here; no compatibility re-export or second decoder is retained.

- Its public API names types of `awaken-agent-contract` 2.0, `awaken-tool-contract` 2.0 and `awaken-runtime-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.
- `BACKGROUND_TASK_STATE_PREFIX` moved to `awaken_agent_contract::thread::background_task`; import it from there (with `background_task_state_key`). It is not re-exported.

### Added

- `BackgroundTask::heartbeat_checkpoint` with `TaskHeartbeatCheckpointOutcome` and `TaskHeartbeatCheckpointError`.
- `BackgroundTaskConfig::validate`.
