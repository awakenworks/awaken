# Effects

Effects are typed, fire-and-forget side-effect events. Unlike
[scheduled actions](scheduled-actions.md) (which execute within a phase
convergence loop and can cascade), effects are dispatched **after commit** and
are **terminal** -- handlers cannot produce new `StateCommand`s, actions, or
effects.

Typical use cases: audit logging, external webhook calls, metric emission,
notification delivery.

## EffectSpec trait

Every effect type implements `EffectSpec`:

```rust,ignore
pub trait EffectSpec: 'static + Send + Sync {
    /// Unique string identifier for this effect kind.
    const KEY: &'static str;

    /// The payload carried by the effect.
    /// Must be serializable so the runtime can store it as JSON internally.
    type Payload: Serialize + DeserializeOwned + Send + Sync + 'static;
}
```

**Crate path:** `awaken::model::EffectSpec` (re-exported from `awaken-contract`)

`KEY` must be globally unique across all registered effects. Convention:
`"<plugin>.<effect_name>"`, e.g. `"audit.record"`.

## Emitting effects

Call `StateCommand::emit::<E>(payload)` from any hook or action handler:

```rust,ignore
use awaken::{StateCommand, StateError};

async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
    let mut cmd = StateCommand::new();
    cmd.emit::<AuditEffect>(AuditPayload {
        action: "user_login".into(),
        actor: "agent-1".into(),
    })?;
    Ok(cmd)
}
```

Effects are collected in `StateCommand::effects` and dispatched only after the
command's state mutations are committed. Tools can also emit effects by including
them in the `StateCommand` returned alongside a `ToolResult`.

## TypedEffect wrapper

`TypedEffect` is the runtime's type-erased envelope for effects:

```rust,ignore
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TypedEffect {
    pub key: String,
    pub payload: JsonValue,
}
```

Two methods bridge between the typed and erased worlds:

- `TypedEffect::from_spec::<E>(payload)` -- serializes a typed payload into a
  `TypedEffect`. Called internally by `StateCommand::emit`.
- `TypedEffect::decode::<E>()` -- deserializes the JSON payload back into the
  concrete `E::Payload` type.

You rarely need to use `TypedEffect` directly; `StateCommand::emit` and the
handler trait handle serialization transparently.

## Registering effect handlers

Effect handlers implement `TypedEffectHandler<E>`:

```rust,ignore
#[async_trait]
pub trait TypedEffectHandler<E>: Send + Sync + 'static
where
    E: EffectSpec,
{
    async fn handle_typed(
        &self,
        payload: E::Payload,
        snapshot: &Snapshot,
    ) -> Result<(), String>;
}
```

Key points:

- The handler receives the **post-commit** `Snapshot`, so it sees the state that
  includes the mutations from the command that emitted the effect.
- The return type is `Result<(), String>` -- not `Result<(), StateError>`.
  Handlers report errors as plain strings; the runtime logs them but does not
  propagate them.

Register a handler in your plugin's `register()` method:

```rust,ignore
fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
    r.register_effect::<AuditEffect, _>(AuditEffectHandler)?;
    Ok(())
}
```

Duplicate registrations (same `E::KEY`) produce
`StateError::EffectHandlerAlreadyRegistered`.

## Dispatch lifecycle

1. **Collect** -- A hook, action handler, or tool calls `cmd.emit::<E>(payload)`.
   The `TypedEffect` is appended to `StateCommand::effects`.

2. **Validate** -- When `submit_command` processes the command, every effect key
   is checked against the registered handlers. If any key has **no** registered
   handler, the command is rejected with `StateError::UnknownEffectHandler`
   before any state is committed. This is a fail-fast guarantee.

3. **Commit** -- State mutations (`MutationBatch`) are committed to the store.

4. **Dispatch** -- After a successful commit, each effect is dispatched to its
   handler via `handle_typed(payload, snapshot)`. The snapshot reflects post-commit
   state.

5. **Error handling** -- Handler failures are logged and counted in
   `EffectDispatchReport` but do **not** roll back the commit or block
   subsequent effects. The runtime continues dispatching remaining effects.

```text
Hook / Tool                         Runtime
    |                                 |
    |-- StateCommand (with effects) ->|
    |                                 |-- validate all effect keys
    |                                 |   (fail-fast if unknown)
    |                                 |-- commit state mutations
    |                                 |-- dispatch effects sequentially
    |                                 |      handler(payload, snapshot)
    |                                 |-- return SubmitCommandReport
    |<--------------------------------|
```

## Worked example

Define an effect:

```rust,ignore
use awaken::EffectSpec;
use serde::{Deserialize, Serialize};

/// Payload for audit log entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditPayload {
    pub action: String,
    pub actor: String,
}

/// Effect spec for audit logging.
pub struct AuditEffect;

impl EffectSpec for AuditEffect {
    const KEY: &'static str = "audit.record";
    type Payload = AuditPayload;
}
```

Emit it from a phase hook:

```rust,ignore
use async_trait::async_trait;
use awaken::{PhaseContext, PhaseHook, StateCommand, StateError};

pub struct AuditHook;

#[async_trait]
impl PhaseHook for AuditHook {
    async fn run(&self, ctx: &PhaseContext) -> Result<StateCommand, StateError> {
        let mut cmd = StateCommand::new();
        cmd.emit::<AuditEffect>(AuditPayload {
            action: "phase_entered".into(),
            actor: "system".into(),
        })?;
        Ok(cmd)
    }
}
```

Handle the effect:

```rust,ignore
use async_trait::async_trait;
use awaken::{Snapshot, TypedEffectHandler};

pub struct AuditEffectHandler;

#[async_trait]
impl TypedEffectHandler<AuditEffect> for AuditEffectHandler {
    async fn handle_typed(
        &self,
        payload: AuditPayload,
        _snapshot: &Snapshot,
    ) -> Result<(), String> {
        tracing::info!(
            action = %payload.action,
            actor = %payload.actor,
            "audit effect dispatched"
        );
        // In production: write to external audit store, send webhook, etc.
        Ok(())
    }
}
```

Wire it all together in a plugin:

```rust,ignore
use awaken::{Plugin, PluginDescriptor, PluginRegistrar, StateError};
use awaken::model::Phase;

pub struct AuditPlugin;

impl Plugin for AuditPlugin {
    fn descriptor(&self) -> PluginDescriptor {
        PluginDescriptor::new("audit", "Audit logging via effects")
    }

    fn register(&self, r: &mut PluginRegistrar) -> Result<(), StateError> {
        r.register_effect::<AuditEffect, _>(AuditEffectHandler)?;
        r.register_phase_hook("audit", Phase::RunStart, AuditHook)?;
        Ok(())
    }
}
```

## Effects vs Scheduled Actions

| | Effects | Scheduled Actions |
|---|---|---|
| **Timing** | Post-commit | Within phase convergence loop |
| **Can cascade** | No | Yes (handlers return `StateCommand`) |
| **Can produce StateCommand** | No | Yes |
| **Failure handling** | Logged, non-blocking | Error propagated to caller |
| **State visibility** | Post-commit snapshot | Pre-commit context |
| **Use case** | External I/O, logging, metrics | Internal control flow, state manipulation |

Choose **effects** when you need to trigger external side-effects that should not
influence the agent's state convergence. Choose **scheduled actions** when the
handler needs to mutate state or schedule further work.

## See also

- [Plugin Internals](../explanation/plugin-internals.md)
- [Scheduled Actions](scheduled-actions.md)
