use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::join_all;
use futures::lock::Mutex;

use crate::error::StateError;
use crate::model::{
    FailedScheduledAction, FailedScheduledActionUpdate, FailedScheduledActions,
    PendingScheduledActions, Phase, ScheduledActionEnvelope, ScheduledActionQueueUpdate,
    TypedEffect,
};
use crate::state::{MutationBatch, Snapshot, StateCommand, StateStore};

use super::PhaseContext;
use super::env::ExecutionEnv;
use super::handlers::{ToolPermissionResult, aggregate_tool_permissions};
use super::registry::RuntimeQueuePlugin;
use super::reports::{
    DEFAULT_MAX_PHASE_ROUNDS, EffectDispatchReport, PhaseRunReport, SubmitCommandReport,
};

#[derive(Clone)]
pub struct PhaseRuntime {
    store: StateStore,
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
            execution_lock: Arc::new(Mutex::new(())),
            next_id: Arc::new(AtomicU64::new(1)),
        })
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    pub async fn submit_command(
        &self,
        env: &ExecutionEnv,
        command: StateCommand,
    ) -> Result<SubmitCommandReport, StateError> {
        let _guard = self.execution_lock.lock().await;
        self.submit_command_inner(env, command).await
    }

    pub async fn run_phase(
        &self,
        env: &ExecutionEnv,
        phase: Phase,
    ) -> Result<PhaseRunReport, StateError> {
        self.run_phase_with_limit(env, phase, DEFAULT_MAX_PHASE_ROUNDS)
            .await
    }

    pub async fn run_phase_with_context(
        &self,
        env: &ExecutionEnv,
        ctx: PhaseContext,
    ) -> Result<PhaseRunReport, StateError> {
        self.run_phase_ctx_inner(env, ctx, DEFAULT_MAX_PHASE_ROUNDS)
            .await
    }

    /// Run phase hooks without committing — return the combined StateCommand.
    pub async fn collect_commands(
        &self,
        env: &ExecutionEnv,
        ctx: PhaseContext,
    ) -> Result<StateCommand, StateError> {
        self.run_hooks_collect(env, ctx).await
    }

    pub async fn run_phase_with_limit(
        &self,
        env: &ExecutionEnv,
        phase: Phase,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        let ctx = PhaseContext::new(phase, self.store.snapshot());
        self.run_phase_ctx_inner(env, ctx, max_rounds).await
    }

    async fn run_phase_ctx_inner(
        &self,
        env: &ExecutionEnv,
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

        let hook_snapshot = self.store.snapshot();
        let hook_command = self
            .gather_hook_commands(env, base_ctx.clone(), hook_snapshot)
            .await?;
        if !hook_command.is_empty() {
            total_effects += hook_command.effects.len();
            let report = self.submit_command_inner(env, hook_command).await?;
            effect_report.attempted += report.effect_report.attempted;
            effect_report.dispatched += report.effect_report.dispatched;
            effect_report.failed += report.effect_report.failed;
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
                let handler = env
                    .scheduled_action_handlers
                    .get(&envelope.action.key)
                    .cloned();

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
                match self.submit_command_inner(env, command).await {
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
        env: &ExecutionEnv,
        mut command: StateCommand,
    ) -> Result<SubmitCommandReport, StateError> {
        // Validate all action/effect keys have registered handlers
        for action in &command.scheduled_actions {
            if !env.scheduled_action_handlers.contains_key(&action.key) {
                return Err(StateError::UnknownScheduledActionHandler {
                    key: action.key.clone(),
                });
            }
        }
        for effect in &command.effects {
            if !env.effect_handlers.contains_key(&effect.key) {
                return Err(StateError::UnknownEffectHandler {
                    key: effect.key.clone(),
                });
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
        let effect_report = self.dispatch_effects(env, &effects, &snapshot).await;
        Ok(SubmitCommandReport {
            revision,
            effect_report,
        })
    }

    async fn dispatch_effects(
        &self,
        env: &ExecutionEnv,
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
            let Some(handler) = env.effect_handlers.get(&effect.key) else {
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

    /// Check tool permission by running all permission checkers in the env.
    pub async fn check_tool_permission(
        &self,
        env: &ExecutionEnv,
        ctx: &PhaseContext,
    ) -> Result<ToolPermissionResult, StateError> {
        let mut decisions = Vec::with_capacity(env.tool_permission_checkers.len());
        for checker in &env.tool_permission_checkers {
            let check_ctx = ctx.clone().with_snapshot(self.store.snapshot());
            decisions.push(checker.check(&check_ctx).await?);
        }
        Ok(aggregate_tool_permissions(&decisions))
    }

    /// Run phase hooks, collecting their commands without committing.
    async fn run_hooks_collect(
        &self,
        env: &ExecutionEnv,
        ctx: PhaseContext,
    ) -> Result<StateCommand, StateError> {
        self.gather_hook_commands(env, ctx, self.store.snapshot())
            .await
    }

    async fn gather_hook_commands(
        &self,
        env: &ExecutionEnv,
        base_ctx: PhaseContext,
        snapshot: Snapshot,
    ) -> Result<StateCommand, StateError> {
        let hooks = env.hooks_for_phase(base_ctx.phase);
        let active_plugins = &base_ctx.profile.active_plugins;
        let filtered_hooks: Vec<_> = hooks
            .iter()
            .filter(|tagged| {
                active_plugins.is_empty() || active_plugins.contains(&tagged.plugin_id)
            })
            .collect();

        let results = join_all(filtered_hooks.into_iter().map(|tagged| {
            let hook = tagged.hook.clone();
            let hook_snapshot = snapshot.clone();
            let hook_ctx = base_ctx.clone().with_snapshot(hook_snapshot.clone());
            async move {
                let mut cmd = hook.run(&hook_ctx).await?;
                if cmd.base_revision().is_none() {
                    cmd = cmd.with_base_revision(hook_snapshot.revision());
                }
                Ok::<StateCommand, StateError>(cmd)
            }
        }))
        .await;

        let mut commands = Vec::new();
        for result in results {
            let cmd = result?;
            if !cmd.is_empty() {
                commands.push(cmd);
            }
        }

        self.store.merge_all_commands(commands)
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
