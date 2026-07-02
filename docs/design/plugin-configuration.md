# Plugin Configuration

A plugin contributes behavior (tools, hooks, gates, guards) under its declared
`CapabilityBound`. Some plugins need that behavior to differ **per agent** — the
tool state machine, for example, enforces a different machine set for a "writer"
agent than for a "reviewer". This document owns how an agent carries per-plugin
configuration, how each plugin compiles its contributions from its own config at
resolve time, how a bad configuration fails closed, and how a frontend perceives
each plugin's configuration schema to author it.

The design keeps one rule above all: **the plugin's strong config type is the
single source of truth**. Structural validation is its `serde` decode, semantic
validation is its own compile step, and the editor schema is *derived* from the
same type — never hand-authored, so the three can never drift.

## Principles

- **One source of truth.** A plugin owns a strong Rust config type. Decoding is
  structural validation; compiling that type into the plugin's model is semantic
  validation; the JSON Schema is derived from the type.
- **Validator equals applier.** Publish-time validation is a *dry run* of the
  same `resolve` the runtime uses; there is no separate `validate` method that
  could drift from what actually runs.
- **Tell, don't ask.** The runtime hands a plugin *its own* config section; the
  plugin does not reach into a context to pull a keyed value.
- **Kernel stays minimal and neutral.** The kernel transports raw JSON config and
  invokes resolve; it never names a schema or any plugin's config shape (G10).
- **Fail closed at the boundary.** An invalid section fails the publish (and, as
  a backstop, the run) closed, never silently.

## Model

### The config carrier

Per-plugin configuration is a value object on the resolved decision surface —
raw JSON keyed by plugin id, so the kernel transports it without knowing any
plugin's shape (data-only, G3):

```rust
pub struct ResolvedSpec {
    // …existing fields…
    /// Per-plugin configuration, keyed by plugin id. Raw JSON: the kernel
    /// carries it across the config→runtime edge without naming any plugin's
    /// config type. A plugin whose id is absent runs with its defaults.
    #[serde(default)]
    pub plugin_config: BTreeMap<String, serde_json::Value>,
}
```

The agent config authors these sections; the compile step copies them into
`ResolvedSpec.plugin_config`. `PluginManifest.config_sections` already declares
which section ids a plugin owns.

### Config-aware resolve

The runtime hands each active plugin its own section. Config-agnostic plugins are
untouched — a default method delegates to the existing `resolve`:

```rust
pub trait Plugin: Send + Sync {
    fn manifest(&self) -> PluginManifest;

    /// Config-agnostic contributions (the default behavior).
    fn resolve(&self) -> Contributions;

    /// Config-aware resolve. The runtime passes the plugin its own config section
    /// (by manifest id), or `None`. The default ignores it; a configurable plugin
    /// overrides this and fails closed on a malformed section.
    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let _ = config;
        Ok(self.resolve())
    }

    fn live_version(&self) -> Option<u64> {
        None
    }
}

#[derive(Debug, thiserror::Error)]
pub enum PluginConfigError {
    #[error("malformed config for plugin {plugin}: {message}")]
    Malformed { plugin: String, message: String },
}
```

The runtime, when merging active plugins, calls
`plugin.resolve_configured(spec.plugin_config.get(&manifest.id))?` and fails the
run closed (`Failure::CapabilityBound`-style) on `Err`. Only plugins that opt in
override the default, so existing plugins need no change.

### Validation — the validator is the applier

There is no separate validation method and no JSON-Schema validation in the
kernel. A section is valid **iff** the plugin can resolve it:

- **Publish time (authoritative).** The config aggregate enforces its invariant
  by dry-running `resolve_configured(Some(candidate))` and rejecting the publish
  on `Err`. An invalid section never reaches a run.
- **Run time (backstop).** The engine's `resolve_configured` also returns
  `Result`, so a section that somehow reaches a run fails it closed.

Because publish validation and run application are the *same function*, "what is
valid" and "what runs" cannot diverge.

### Schema — derived, for the frontend

A frontend needs each plugin's config schema to render a form and give authoring
feedback. The schema is **derived from the same config type**, never authored:

```rust
// In the extension crate, gated so the runtime hot path never links schemars.
#[cfg(feature = "schema")]
pub fn config_schema() -> serde_json::Value {
    serde_json::to_value(schemars::schema_for!(StateMachineConfig)).unwrap()
}
```

`schemars` honors the same `serde` attributes (`rename_all`, `default`,
`untagged`), so `untagged` enums become `anyOf`, defaulted fields become
optional, and renamed variants match — no second definition. A string-encoded DSL
field (a tool-call pattern, a key template) appears as `type: string`; enrich it
with `#[schemars(description = …, example = …)]` for authoring hints, while its
real validation stays in the plugin's compile.

The schema travels on the existing capability catalog — one new field:

```rust
pub struct PluginCapability {
    pub id: String,
    pub schema_keys: Vec<String>,
    /// JSON Schema for this plugin's config section, derived from its config
    /// type. Absent when the plugin has no config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_schema: Option<serde_json::Value>,
}
```

The composition/config side fills `config_schema` when it builds the catalog (it
already composes the plugins, so it knows the id→schema mapping); a protocol
adapter serves the catalog to the frontend as a public DTO (G19). The runtime
`Plugin` trait gains nothing — schema lives in the extension (derived) and the
capability data (carried), not the kernel.

### Client vs server validation

| Where | Uses | Authority |
|---|---|---|
| Frontend | the derived JSON Schema for live form validation | UX only — never trusted |
| Publish (server) | dry-run `resolve_configured(candidate)` | the single authority |

The frontend schema and the server validator are both projections of one config
type, so they agree by construction; the server never trusts the client.

## The state machine as the first consumer

The tool state machine registers once with no baked-in machines and compiles its
machine set from the agent's section at resolve:

```rust
impl Plugin for StateMachinePlugin {
    fn resolve(&self) -> Contributions {
        self.contribute(&self.base)          // base machines only
    }

    fn resolve_configured(
        &self,
        config: Option<&serde_json::Value>,
    ) -> Result<Contributions, PluginConfigError> {
        let configured = match config {
            Some(value) => StateMachineConfig::from_value(value.clone())
                .and_then(StateMachineConfig::into_machines)
                .map_err(|e| PluginConfigError::Malformed {
                    plugin: STATE_MACHINE_PLUGIN_ID.into(),
                    message: e.to_string(),
                })?,
            None => Vec::new(),
        };
        Ok(self.contribute(&merge(&self.base, configured)?))
    }
}
```

No config-key type and no resolve context are needed: the plugin decodes its own
section directly with `serde`. The host registers `StateMachinePlugin::empty()`
once and, for authoring, registers `config_schema()` into the capability catalog.

## Ownership (bounded contexts)

| Context | Owns |
|---|---|
| Agent config (config-store) | the raw sections as data; enforces validity at publish via dry-run resolve |
| Extension | the strong config type, its `serde` decode, its compile, its derived schema |
| Runtime kernel | transporting the raw config value object; invoking `resolve_configured` |
| Protocol adapter | serving the capability catalog (with `config_schema`) to the frontend |

## Plugin Configuration Role Catalog

| Role / component | Responsibility |
|---|---|
| Config carrier (`ResolvedSpec.plugin_config`) | per-plugin raw config value object on the resolved decision surface |
| Config-aware resolve (`Plugin::resolve_configured`) | hands a plugin its own section; returns contributions or fails closed |
| Config validator (dry-run `resolve_configured`) | publish-time invariant of the agent-config aggregate; same function as the applier |
| Schema deriver (`config_schema` in the extension) | derives a JSON Schema from the config type via `schemars`, feature-gated |
| Schema carrier (`PluginCapability.config_schema`) | transports the derived schema to the frontend on the capability catalog |

## Guardrails touched

G3 (only serializable data crosses the config→runtime edge; `plugin_config` is
raw JSON), G10 (the kernel names no schema or plugin config shape), G19 (the
config schema is a public DTO owned by a protocol adapter), G30 (a plugin that
cannot resolve its config contributes nothing — fail closed).

## Verification

- **Config-driven behavior** — two agents activate the same plugin with different
  `plugin_config` sections and get different contributions from one run each.
- **Fail closed** — a malformed section is rejected at publish (dry-run resolve
  `Err`) and, as a backstop, fails a run closed; a well-formed section runs.
- **No drift** — the same config type powers decode, compile, and the derived
  schema; a field added to the type appears in all three without extra edits.
- **Kernel neutral** — the runtime `Plugin` trait and `ResolvedSpec` name no
  schema; `schemars` is absent from the runtime build (feature-gated).
- **Frontend perceives schema** — the capability catalog served by the adapter
  carries each plugin's `config_schema`; client validation matches the server's
  dry-run outcome for the same input.
