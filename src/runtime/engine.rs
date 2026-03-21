use std::any::TypeId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use futures::lock::Mutex;

use crate::error::StateError;
use crate::model::{
    EffectSpec, FailedScheduledAction, FailedScheduledActionUpdate, FailedScheduledActions,
    PendingScheduledActions, Phase, ScheduledActionEnvelope, ScheduledActionQueueUpdate,
    ScheduledActionSpec, TypedEffect,
};
use crate::plugins::{Plugin, PluginRegistrar};
use crate::state::{MutationBatch, Snapshot, StateCommand, StateStore};

use super::PhaseContext;
use super::handlers::{
    PhaseHook, ToolPermissionResult, TypedEffectHandler, TypedScheduledActionHandler,
    aggregate_tool_permissions,
};
use super::registry::{InstalledRuntimePlugin, RuntimeQueuePlugin, RuntimeRegistry};
use super::reports::{
    DEFAULT_MAX_PHASE_ROUNDS, EffectDispatchReport, PhaseRunReport, SubmitCommandReport,
};

#[derive(Clone)]
pub struct PhaseRuntime {
    store: StateStore,
    runtime_registry: Arc<RwLock<RuntimeRegistry>>,
    execution_lock: Arc<Mutex<()>>,
    next_id: Arc<AtomicU64>,
}

impl PhaseRuntime {
    pub fn new(store: StateStore) -> Result<Self, StateError> {
        match store.install_plugin(RuntimeQueuePlugin) {
            Ok(()) => {}
            Err(StateError::PluginAlreadyInstalled { .. }) => {}
            Err(err) => return Err(err),
        }

        Ok(Self {
            store,
            runtime_registry: Arc::new(RwLock::new(RuntimeRegistry::default())),
            execution_lock: Arc::new(Mutex::new(())),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    pub fn register_scheduled_action<A, H>(&self, handler: H) -> Result<(), StateError>
    where
        A: ScheduledActionSpec,
        H: TypedScheduledActionHandler<A>,
    {
        let mut registrar = PluginRegistrar::new();
        registrar.register_scheduled_action::<A, H>(handler)?;
        self.commit_runtime_registrations(None, registrar)
    }

    pub fn register_effect<E, H>(&self, handler: H) -> Result<(), StateError>
    where
        E: EffectSpec,
        H: TypedEffectHandler<E>,
    {
        let mut registrar = PluginRegistrar::new();
        registrar.register_effect::<E, H>(handler)?;
        self.commit_runtime_registrations(None, registrar)
    }

    pub fn install_plugin<P>(&self, plugin: P) -> Result<(), StateError>
    where
        P: Plugin,
    {
        let mut registrar = PluginRegistrar::new();
        plugin.register(&mut registrar)?;
        let plugin_type_id = TypeId::of::<P>();

        let keys = std::mem::take(&mut registrar.keys);
        let plugin_arc: Arc<dyn Plugin> = Arc::new(plugin);
        self.store
            .install_plugin_with_keys(plugin_type_id, plugin_arc, keys)?;

        if let Err(err) = self.commit_runtime_registrations(Some(plugin_type_id), registrar) {
            let _ = self.store.uninstall_plugin::<P>();
            return Err(err);
        }
        Ok(())
    }

    pub fn uninstall_plugin<P>(&self) -> Result<(), StateError>
    where
        P: Plugin,
    {
        self.store.uninstall_plugin::<P>()?;
        self.remove_runtime_plugin::<P>();
        Ok(())
    }

    pub async fn submit_command(
        &self,
        command: StateCommand,
    ) -> Result<SubmitCommandReport, StateError> {
        let _guard = self.execution_lock.lock().await;
        self.submit_command_inner(command).await
    }

    pub async fn run_phase(&self, phase: Phase) -> Result<PhaseRunReport, StateError> {
        self.run_phase_with_limit(phase, DEFAULT_MAX_PHASE_ROUNDS)
            .await
    }

    pub async fn run_phase_with_context(
        &self,
        ctx: PhaseContext,
    ) -> Result<PhaseRunReport, StateError> {
        self.run_phase_ctx_inner(ctx, DEFAULT_MAX_PHASE_ROUNDS)
            .await
    }

    /// Run phase hooks without committing — return the combined StateCommand.
    pub async fn collect_commands(&self, ctx: PhaseContext) -> Result<StateCommand, StateError> {
        self.run_hooks_collect(ctx).await
    }

    pub async fn run_phase_with_limit(
        &self,
        phase: Phase,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        let ctx = PhaseContext::new(phase, self.store.snapshot());
        self.run_phase_ctx_inner(ctx, max_rounds).await
    }

    async fn run_phase_ctx_inner(
        &self,
        base_ctx: PhaseContext,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        let phase = base_ctx.phase;
        let _guard = self.execution_lock.lock().await;
        let mut total_processed = 0;
        let mut total_skipped = 0;
        let mut total_failed = 0;
        let mut total_effects = 0;
        let mut effect_report = EffectDispatchReport {
            attempted: 0,
            dispatched: 0,
            failed: 0,
        };
        let mut rounds = 0;

        // Phase hooks: filtered by ctx.profile.active_plugins
        let hooks = self.resolve_hooks(base_ctx.phase, &base_ctx.profile.active_plugins);
        for hook in &hooks {
            let ctx = base_ctx.clone().with_snapshot(self.store.snapshot());
            let command = hook.run(&ctx).await?;
            if !command.is_empty() {
                total_effects += command.effects.len();
                let report = self.submit_command_inner(command).await?;
                effect_report.attempted += report.effect_report.attempted;
                effect_report.dispatched += report.effect_report.dispatched;
                effect_report.failed += report.effect_report.failed;
            }
        }

        loop {
            rounds += 1;
            if rounds > max_rounds {
                return Err(StateError::PhaseRunLoopExceeded { phase, max_rounds });
            }

            let queued = self
                .store
                .read::<PendingScheduledActions>()
                .unwrap_or_default();

            let matching: Vec<_> = queued
                .into_iter()
                .filter(|envelope| envelope.action.phase == phase)
                .collect();

            if matching.is_empty() {
                if rounds == 1 {
                    total_skipped = self
                        .store
                        .read::<PendingScheduledActions>()
                        .unwrap_or_default()
                        .iter()
                        .filter(|envelope| envelope.action.phase != phase)
                        .count();
                }
                break;
            }

            for envelope in matching {
                let handler = {
                    let registry = self
                        .runtime_registry
                        .read()
                        .expect("runtime registry lock poisoned");
                    registry
                        .scheduled_action_handlers
                        .get(&envelope.action.key)
                        .cloned()
                };

                let Some(handler) = handler else {
                    let key = envelope.action.key.clone();
                    self.dead_letter(envelope, format!("no action handler registered for {key}"))?;
                    total_failed += 1;
                    continue;
                };

                let ctx = base_ctx.clone().with_snapshot(self.store.snapshot());
                let mut command = match handler
                    .handle_erased(&ctx, envelope.action.payload.clone())
                    .await
                {
                    Ok(command) => command,
                    Err(err) => {
                        self.dead_letter(envelope, err.to_string())?;
                        total_failed += 1;
                        continue;
                    }
                };
                total_effects += command.effects.len();
                command.patch.update::<PendingScheduledActions>(
                    ScheduledActionQueueUpdate::Remove { id: envelope.id },
                );
                match self.submit_command_inner(command).await {
                    Ok(report) => {
                        total_processed += 1;
                        effect_report.attempted += report.effect_report.attempted;
                        effect_report.dispatched += report.effect_report.dispatched;
                        effect_report.failed += report.effect_report.failed;
                    }
                    Err(err) => {
                        self.dead_letter(
                            envelope,
                            format!("failed to submit action command: {err}"),
                        )?;
                        total_failed += 1;
                    }
                }
            }
        }

        Ok(PhaseRunReport {
            phase,
            rounds,
            processed_scheduled_actions: total_processed,
            skipped_scheduled_actions: total_skipped,
            failed_scheduled_actions: total_failed,
            generated_effects: total_effects,
            effect_report,
        })
    }

    async fn submit_command_inner(
        &self,
        mut command: StateCommand,
    ) -> Result<SubmitCommandReport, StateError> {
        {
            let registry = self
                .runtime_registry
                .read()
                .expect("runtime registry lock poisoned");
            for action in &command.scheduled_actions {
                if !registry.scheduled_action_handlers.contains_key(&action.key) {
                    return Err(StateError::UnknownScheduledActionHandler {
                        key: action.key.clone(),
                    });
                }
            }
            for effect in &command.effects {
                if !registry.effect_handlers.contains_key(&effect.key) {
                    return Err(StateError::UnknownEffectHandler {
                        key: effect.key.clone(),
                    });
                }
            }
        }

        for action in command.scheduled_actions.drain(..) {
            let entry = ScheduledActionEnvelope {
                id: self.next_id.fetch_add(1, Ordering::SeqCst),
                action,
            };
            tracing::debug!(
                id = entry.id,
                phase = ?entry.action.phase,
                key = %entry.action.key,
                "scheduled action enqueued"
            );
            command
                .patch
                .update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(entry));
        }

        let mut effects = Vec::new();
        for effect in command.effects.drain(..) {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            tracing::debug!(id, key = %effect.key, "effect dispatching");
            effects.push(effect);
        }

        let revision = self.store.commit(command.patch)?;
        let snapshot = self.store.snapshot();
        let effect_report = self.dispatch_effects(&effects, &snapshot).await;
        Ok(SubmitCommandReport {
            revision,
            effect_report,
        })
    }

    fn commit_runtime_registrations(
        &self,
        plugin_type_id: Option<TypeId>,
        mut registrar: PluginRegistrar,
    ) -> Result<(), StateError> {
        let mut registry = self
            .runtime_registry
            .write()
            .expect("runtime registry lock poisoned");

        registry.validate_registrar(plugin_type_id, &registrar)?;

        let mut installed_plugin = InstalledRuntimePlugin::default();
        for entry in registrar.scheduled_actions.drain(..) {
            installed_plugin
                .scheduled_action_keys
                .push(entry.key.clone());
            registry
                .scheduled_action_handlers
                .insert(entry.key, entry.handler);
        }

        for entry in registrar.effects.drain(..) {
            installed_plugin.effect_keys.push(entry.key.clone());
            registry.effect_handlers.insert(entry.key, entry.handler);
        }

        for entry in registrar.phase_hooks.drain(..) {
            let hook_id = registry.next_hook_id;
            registry.next_hook_id += 1;
            installed_plugin.phase_hook_ids.push((entry.phase, hook_id));
            registry.phase_hooks.entry(entry.phase).or_default().push((
                hook_id,
                entry.plugin_id,
                entry.hook,
            ));
        }

        for entry in registrar.tool_permissions.drain(..) {
            let checker_id = registry.next_hook_id;
            registry.next_hook_id += 1;
            installed_plugin.tool_permission_ids.push(checker_id);
            registry
                .tool_permission_checkers
                .push((checker_id, entry.plugin_id, entry.checker));
        }

        if let Some(plugin_type_id) = plugin_type_id {
            registry
                .installed_plugins
                .insert(plugin_type_id, installed_plugin);
        }

        Ok(())
    }

    fn remove_runtime_plugin<P>(&self)
    where
        P: Plugin,
    {
        let plugin_type_id = TypeId::of::<P>();
        let mut registry = self
            .runtime_registry
            .write()
            .expect("runtime registry lock poisoned");
        let Some(installed) = registry.installed_plugins.remove(&plugin_type_id) else {
            return;
        };
        for key in installed.scheduled_action_keys {
            registry.scheduled_action_handlers.remove(&key);
        }
        for key in installed.effect_keys {
            registry.effect_handlers.remove(&key);
        }
        for (phase, hook_id) in installed.phase_hook_ids {
            if let Some(hooks) = registry.phase_hooks.get_mut(&phase) {
                hooks.retain(|(id, _, _)| *id != hook_id);
            }
        }
        for checker_id in installed.tool_permission_ids {
            registry
                .tool_permission_checkers
                .retain(|(id, _, _)| *id != checker_id);
        }
    }

    async fn dispatch_effects(
        &self,
        effects: &[TypedEffect],
        snapshot: &Snapshot,
    ) -> EffectDispatchReport {
        let mut report = EffectDispatchReport {
            attempted: 0,
            dispatched: 0,
            failed: 0,
        };

        for effect in effects {
            report.attempted += 1;
            let handler = {
                let registry = self
                    .runtime_registry
                    .read()
                    .expect("runtime registry lock poisoned");
                registry.effect_handlers.get(&effect.key).cloned()
            };

            let Some(handler) = handler else {
                report.failed += 1;
                continue;
            };

            match handler
                .handle_erased(effect.payload.clone(), snapshot)
                .await
            {
                Ok(()) => report.dispatched += 1,
                Err(_) => report.failed += 1,
            }
        }

        report
    }

    /// Check tool permission by running all registered ToolPermissionCheckers.
    pub async fn check_tool_permission(
        &self,
        ctx: &PhaseContext,
    ) -> Result<ToolPermissionResult, StateError> {
        let active = &ctx.profile.active_plugins;
        let checkers: Vec<_> = {
            let registry = self
                .runtime_registry
                .read()
                .expect("runtime registry lock poisoned");
            registry
                .tool_permission_checkers
                .iter()
                .filter(|(_, plugin_id, _)| active.is_empty() || active.contains(plugin_id))
                .map(|(_, _, checker)| Arc::clone(checker))
                .collect()
        };

        let mut decisions = Vec::with_capacity(checkers.len());
        for checker in &checkers {
            let check_ctx = ctx.clone().with_snapshot(self.store.snapshot());
            decisions.push(checker.check(&check_ctx).await?);
        }

        Ok(aggregate_tool_permissions(&decisions))
    }

    /// Resolve hooks for a phase, filtered by active plugins.
    fn resolve_hooks(
        &self,
        phase: Phase,
        active_plugins: &std::collections::HashSet<String>,
    ) -> Vec<Arc<dyn PhaseHook>> {
        let registry = self
            .runtime_registry
            .read()
            .expect("runtime registry lock poisoned");
        registry
            .phase_hooks
            .get(&phase)
            .map(|hooks| {
                hooks
                    .iter()
                    .filter(|(_, plugin_id, _)| {
                        active_plugins.is_empty() || active_plugins.contains(plugin_id)
                    })
                    .map(|(_, _, hook)| Arc::clone(hook))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Run phase hooks, collecting their commands without committing.
    async fn run_hooks_collect(&self, ctx: PhaseContext) -> Result<StateCommand, StateError> {
        let hooks = self.resolve_hooks(ctx.phase, &ctx.profile.active_plugins);
        let mut combined = StateCommand::new();
        for hook in hooks {
            let hook_ctx = ctx.clone().with_snapshot(self.store.snapshot());
            let cmd = hook.run(&hook_ctx).await?;
            if !cmd.is_empty() {
                combined.extend(cmd)?;
            }
        }
        Ok(combined)
    }

    fn dead_letter(
        &self,
        envelope: ScheduledActionEnvelope,
        error: String,
    ) -> Result<(), StateError> {
        let mut patch = MutationBatch::new();
        patch.update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Remove {
            id: envelope.id,
        });
        patch.update::<FailedScheduledActions>(FailedScheduledActionUpdate::Push(
            FailedScheduledAction {
                id: envelope.id,
                action: envelope.action,
                error,
            },
        ));
        self.store.commit(patch).map(|_| ())
    }
}
