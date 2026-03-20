use crate::foundation::*;
use crate::patch::{MutationBatch, PhaseContext, StateCommand};
use crate::store::StateStore;
use std::any::TypeId;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use super::registry::{
    InstalledRuntimePlugin, RuntimePlugin, RuntimePluginRegistrar, RuntimeQueuePlugin,
    RuntimeRegistry,
};
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
        H: super::handlers::TypedScheduledActionHandler<A>,
    {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        let mut registrar = RuntimePluginRegistrar::new();
        registrar.register_scheduled_action::<A, H>(handler)?;
        self.commit_runtime_registrations(None, registrar)
    }

    pub fn register_effect<E, H>(&self, handler: H) -> Result<(), StateError>
    where
        E: EffectSpec,
        H: super::handlers::TypedEffectHandler<E>,
    {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        let mut registrar = RuntimePluginRegistrar::new();
        registrar.register_effect::<E, H>(handler)?;
        self.commit_runtime_registrations(None, registrar)
    }

    pub fn install_plugin<P>(&self, plugin: P) -> Result<(), StateError>
    where
        P: RuntimePlugin,
    {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        let mut registrar = RuntimePluginRegistrar::new();
        plugin.register_runtime(&mut registrar)?;
        let plugin_type_id = TypeId::of::<P>();
        self.ensure_runtime_registrations_available(Some(plugin_type_id), &registrar)?;
        self.store.install_plugin(plugin)?;
        if let Err(err) = self.commit_runtime_registrations(Some(plugin_type_id), registrar) {
            let _ = self.store.uninstall_plugin::<P>();
            return Err(err);
        }
        Ok(())
    }

    pub fn uninstall_plugin<P>(&self) -> Result<(), StateError>
    where
        P: RuntimePlugin,
    {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        self.store.uninstall_plugin::<P>()?;
        self.remove_runtime_plugin::<P>();
        Ok(())
    }

    pub fn submit_command(&self, command: StateCommand) -> Result<SubmitCommandReport, StateError> {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        self.submit_command_inner(command)
    }

    pub fn trim_logs(&self, keep: usize) -> Result<(), StateError> {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        let mut patch = MutationBatch::new();
        patch.update::<ScheduledActionLog>(ScheduledActionLogUpdate::TrimToLast { keep });
        patch.update::<EffectLog>(EffectLogUpdate::TrimToLast { keep });
        self.store.commit(patch).map(|_| ())
    }

    pub fn clear_logs(&self) -> Result<(), StateError> {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
        let mut patch = MutationBatch::new();
        patch.update::<ScheduledActionLog>(ScheduledActionLogUpdate::Clear);
        patch.update::<EffectLog>(EffectLogUpdate::Clear);
        self.store.commit(patch).map(|_| ())
    }

    pub fn run_phase(&self, phase: Phase) -> Result<PhaseRunReport, StateError> {
        self.run_phase_with_limit(phase, DEFAULT_MAX_PHASE_ROUNDS)
    }

    pub fn run_phase_with_limit(
        &self,
        phase: Phase,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        let _guard = self
            .execution_lock
            .lock()
            .expect("runtime execution lock poisoned");
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

        loop {
            rounds += 1;
            if rounds > max_rounds {
                return Err(StateError::PhaseRunLoopExceeded { phase, max_rounds });
            }

            let queued = self
                .store
                .read_slot::<PendingScheduledActions>()
                .unwrap_or_default();

            let matching: Vec<_> = queued
                .into_iter()
                .filter(|envelope| envelope.action.phase == phase)
                .collect();

            if matching.is_empty() {
                if rounds == 1 {
                    total_skipped = self
                        .store
                        .read_slot::<PendingScheduledActions>()
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

                let ctx = PhaseContext {
                    phase,
                    snapshot: self.store.snapshot(),
                };
                let mut command = match handler.handle_erased(&ctx, envelope.action.payload.clone())
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
                match self.submit_command_inner(command) {
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

    fn submit_command_inner(
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
            let log_entry = ScheduledActionLogEntry {
                id: entry.id,
                phase: entry.action.phase,
                key: entry.action.key.clone(),
            };
            command
                .patch
                .update::<PendingScheduledActions>(ScheduledActionQueueUpdate::Push(entry));
            command
                .patch
                .update::<ScheduledActionLog>(ScheduledActionLogUpdate::Append(log_entry));
        }

        let mut effects = Vec::new();
        for effect in command.effects.drain(..) {
            let id = self.next_id.fetch_add(1, Ordering::SeqCst);
            let log_entry = EffectLogEntry {
                id,
                key: effect.key.clone(),
            };
            command
                .patch
                .update::<EffectLog>(EffectLogUpdate::Append(log_entry));
            effects.push(effect);
        }

        let revision = self.store.commit(command.patch)?;
        let snapshot = self.store.snapshot();
        let effect_report = self.dispatch_effects(&effects, &snapshot);
        Ok(SubmitCommandReport {
            revision,
            effect_report,
        })
    }

    fn ensure_runtime_registrations_available(
        &self,
        plugin_type_id: Option<TypeId>,
        registrar: &RuntimePluginRegistrar,
    ) -> Result<(), StateError> {
        let registry = self
            .runtime_registry
            .read()
            .expect("runtime registry lock poisoned");

        if let Some(plugin_type_id) = plugin_type_id
            && registry.installed_plugins.contains_key(&plugin_type_id)
        {
            return Err(StateError::PluginAlreadyInstalled {
                name: format!("runtime-plugin:{plugin_type_id:?}"),
            });
        }

        for entry in &registrar.scheduled_actions {
            if registry.scheduled_action_handlers.contains_key(&entry.key) {
                return Err(StateError::HandlerAlreadyRegistered {
                    key: entry.key.clone(),
                });
            }
        }

        for entry in &registrar.effects {
            if registry.effect_handlers.contains_key(&entry.key) {
                return Err(StateError::EffectHandlerAlreadyRegistered {
                    key: entry.key.clone(),
                });
            }
        }

        Ok(())
    }

    fn commit_runtime_registrations(
        &self,
        plugin_type_id: Option<TypeId>,
        mut registrar: RuntimePluginRegistrar,
    ) -> Result<(), StateError> {
        let mut registry = self
            .runtime_registry
            .write()
            .expect("runtime registry lock poisoned");

        if let Some(plugin_type_id) = plugin_type_id
            && registry.installed_plugins.contains_key(&plugin_type_id)
        {
            return Err(StateError::PluginAlreadyInstalled {
                name: format!("runtime-plugin:{plugin_type_id:?}"),
            });
        }

        for entry in &registrar.scheduled_actions {
            if registry.scheduled_action_handlers.contains_key(&entry.key) {
                return Err(StateError::HandlerAlreadyRegistered {
                    key: entry.key.clone(),
                });
            }
        }

        for entry in &registrar.effects {
            if registry.effect_handlers.contains_key(&entry.key) {
                return Err(StateError::EffectHandlerAlreadyRegistered {
                    key: entry.key.clone(),
                });
            }
        }

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

        if let Some(plugin_type_id) = plugin_type_id {
            registry
                .installed_plugins
                .insert(plugin_type_id, installed_plugin);
        }

        Ok(())
    }

    fn remove_runtime_plugin<P>(&self)
    where
        P: RuntimePlugin,
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
    }

    fn dispatch_effects(
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

            match handler.handle_erased(effect.payload.clone(), snapshot) {
                Ok(()) => report.dispatched += 1,
                Err(_) => report.failed += 1,
            }
        }

        report
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
