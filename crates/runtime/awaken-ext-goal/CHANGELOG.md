# Changelog

Notable changes to `awaken-ext-goal`. Versions follow the [SDK versioning policy](../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- `controller::Controller::new` takes `&dyn run_settlement::OutcomeRuns` instead of `&dyn RunExecutor`: the controller reads a Run's settlement (and its committed cancel request) through the one port that classifies it.
- `controller::Error` gains the variant `Admission`.
- Its public API names types of `awaken-agent-contract` 2.0 and `awaken-runtime-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.

### Added

- `run_settlement::{RunSettlement, OutcomeRuns}`.
- `Controller::preparation`: a controller for the durable create/CAS path, with no execution capability.
