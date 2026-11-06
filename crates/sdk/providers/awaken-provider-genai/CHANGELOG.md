# Changelog

Notable changes to `awaken-provider-genai`. Versions follow the [SDK versioning policy](../../../../docs/design/sdk-versioning.md); the first entry is compared with the SDK baseline `50b3f3e22d` (`1.0.0`).

## [2.0.0] - Unreleased

### Breaking

- The `genai` dependency moves to the registry release `0.7.0-rc.8`; the re-exported exhaustive `AdapterKind` gains `Mistral`, `Requesty`, `ApiRoute` and `Heabsy`, and the public `Client` is the registry type.
- `GenaiExecutor::effect_scope` and `OpenAiResponsesExecutor::effect_scope` are removed; `provider_model_effect_scope` returns `model_operation::reentry::ModelEffectScope`.
- Its public API names types of `awaken-agent-contract` 2.0 and `awaken-runtime-contract` 2.0; a caller upgrades them together, because a 1.x type is a different type.
