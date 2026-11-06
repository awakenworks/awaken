# Changelog

Notable changes to `awaken-ext-compact`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- Its public API names types of `awaken-agent-contract` 2.0 and `awaken-runtime-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.

### Added

- `CompactAuxiliaryAgents`: the auxiliary Agent declaration of the compact plugin.
