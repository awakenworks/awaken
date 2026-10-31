# Changelog

Notable changes to `awaken-ext-builtin-tools`. Versions follow the [SDK versioning policy](../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- `selected_hand_recovery_modes` is removed; Hand membership is projected by the Platform session configuration.
- The Web configuration types are defined in `awaken-web-policy-contract`; the root paths re-export them unchanged, but plugin methods and `WebSearchRequest::user_location` name the defining types.
- Its public API names types of `awaken-agent-contract` 2.0, `awaken-tool-contract` 2.0 and `awaken-runtime-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.

### Added

- `WebToolConfigurationCompiler`.
