use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::join_all;
use futures::lock::Mutex;

use crate::state::{MergeStrategy, MutationBatch, Snapshot, StateCommand, StateStore};
use awaken_contract::StateError;
use awaken_contract::model::{
    FailedScheduledAction, FailedScheduledActionUpdate, FailedScheduledActions,
    PendingScheduledActions, Phase, ScheduledActionEnvelope, ScheduledActionQueueUpdate,
    TypedEffect,
};

use super::PhaseContext;
use super::env::{ExecutionEnv, TaggedPhaseHook};
use super::queue_plugin::RuntimeQueuePlugin;
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

    /// Run only the EXECUTE stage of a phase (no GATHER/hook execution).
    ///
    /// Processes pending scheduled actions that match the given phase and have
    /// a registered handler. Used when GATHER was already done via `collect_commands`
    /// and the caller has manually submitted the remaining command.
    pub async fn run_execute_loop(
        &self,
        env: &ExecutionEnv,
        ctx: PhaseContext,
    ) -> Result<PhaseRunReport, StateError> {
        self.run_execute_loop_inner(env, ctx, DEFAULT_MAX_PHASE_ROUNDS)
            .await
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

    /// EXECUTE-only inner: same convergence loop as `run_phase_ctx_inner` but
    /// without the GATHER (hook execution) stage.
    async fn run_execute_loop_inner(
        &self,
        env: &ExecutionEnv,
        base_ctx: PhaseContext,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        let _guard = self.execution_lock.lock().await;
        self.execute_scheduled_actions(env, &base_ctx, max_rounds)
            .await
    }

    async fn run_phase_ctx_inner(
        &self,
        env: &ExecutionEnv,
        base_ctx: PhaseContext,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        // Check cancellation at phase entry
        if let Some(token) = base_ctx.cancellation_token.as_ref() {
            if token.is_cancelled() {
                return Err(StateError::Cancelled);
            }
        }

        let _guard = self.execution_lock.lock().await;

        let (hook_effects, hook_effect_report) =
            self.gather_and_commit_hooks(env, &base_ctx).await?;

        // Check cancellation after hooks, before scheduled action execution
        if let Some(token) = base_ctx.cancellation_token.as_ref() {
            if token.is_cancelled() {
                return Err(StateError::Cancelled);
            }
        }

        let mut report = self
            .execute_scheduled_actions(env, &base_ctx, max_rounds)
            .await?;

        report.generated_effects += hook_effects;
        report.effect_report.attempted += hook_effect_report.attempted;
        report.effect_report.dispatched += hook_effect_report.dispatched;
        report.effect_report.failed += hook_effect_report.failed;

        Ok(report)
    }

    /// Convergence loop that processes pending scheduled actions matching the
    /// phase until no more remain. Callers must hold the execution lock.
    async fn execute_scheduled_actions(
        &self,
        env: &ExecutionEnv,
        base_ctx: &PhaseContext,
        max_rounds: usize,
    ) -> Result<PhaseRunReport, StateError> {
        let phase = base_ctx.phase;
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
                .read::<PendingScheduledActions>()
                .unwrap_or_default();

            let matching: Vec<_> = queued
                .into_iter()
                .filter(|envelope| {
                    envelope.action.phase == phase
                        && env
                            .scheduled_action_handlers
                            .contains_key(&envelope.action.key)
                })
                .collect();

            tracing::debug!(phase = ?phase, actions = matching.len(), "execute_scheduled_actions");

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
                    .cloned()
                    .expect("handler existence verified in filter above");

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
        // Validate all action keys have a registered handler.
        for action in &command.scheduled_actions {
            if !env.scheduled_action_handlers.contains_key(&action.key) {
                return Err(StateError::UnknownScheduledActionHandler {
                    key: action.key.clone(),
                });
            }
        }
        // Validate effect keys have registered handlers.
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

    /// Run phase hooks, collecting their commands without committing.
    /// Returns a single merged command; fails on Exclusive conflicts (no auto-fallback).
    async fn run_hooks_collect(
        &self,
        env: &ExecutionEnv,
        ctx: PhaseContext,
    ) -> Result<StateCommand, StateError> {
        let snapshot = self.store.snapshot();
        let hooks = Self::filter_hooks(env, &ctx);
        let indexed = Self::run_hooks_indexed(&hooks, &ctx, &snapshot).await?;
        let commands = indexed.into_iter().map(|(_, cmd)| cmd).collect();
        self.store.merge_all_commands(commands)
    }

    /// Run phase hooks with Exclusive conflict auto-fallback.
    ///
    /// Hooks are pure functions (frozen snapshot in, `StateCommand` out, no side effects),
    /// so re-execution on conflict is always safe.
    ///
    /// Algorithm:
    /// 1. Run all hooks in parallel against a frozen snapshot.
    /// 2. If no Exclusive key overlaps, merge all and commit once.
    /// 3. On conflict: greedily partition into a compatible batch + deferred set.
    ///    Commit the batch, then re-run deferred hooks serially against fresh snapshots.
    async fn gather_and_commit_hooks(
        &self,
        env: &ExecutionEnv,
        base_ctx: &PhaseContext,
    ) -> Result<(usize, EffectDispatchReport), StateError> {
        let hooks = Self::filter_hooks(env, base_ctx);
        if hooks.is_empty() {
            return Ok((
                0,
                EffectDispatchReport {
                    attempted: 0,
                    dispatched: 0,
                    failed: 0,
                },
            ));
        }

        tracing::debug!(phase = ?base_ctx.phase, hooks = hooks.len(), "gather_start");

        let snapshot = self.store.snapshot();
        let indexed = Self::run_hooks_indexed(&hooks, base_ctx, &snapshot).await?;

        if indexed.is_empty() {
            return Ok((
                0,
                EffectDispatchReport {
                    attempted: 0,
                    dispatched: 0,
                    failed: 0,
                },
            ));
        }

        // Fast path: no Exclusive key overlap → merge all, commit once
        let has_conflicts = {
            let registry = self.store.registry.lock();
            has_exclusive_key_overlap(&indexed, |k| registry.merge_strategy(k))
        };

        let mut total_effects = 0;
        let mut effect_report = EffectDispatchReport {
            attempted: 0,
            dispatched: 0,
            failed: 0,
        };

        if !has_conflicts {
            let commands = indexed.into_iter().map(|(_, cmd)| cmd).collect();
            let merged = self.store.merge_all_commands(commands)?;
            if !merged.is_empty() {
                total_effects += merged.effects.len();
                let r = self.submit_command_inner(env, merged).await?;
                effect_report.attempted += r.effect_report.attempted;
                effect_report.dispatched += r.effect_report.dispatched;
                effect_report.failed += r.effect_report.failed;
            }
            return Ok((total_effects, effect_report));
        }

        // Conflict fallback: partition into compatible batch + deferred
        tracing::warn!(phase = ?base_ctx.phase, "exclusive_conflict_fallback");
        let (batch_commands, deferred_indices) = {
            let registry = self.store.registry.lock();
            partition_commands(indexed, |k| registry.merge_strategy(k))
        };

        // Commit the compatible batch
        if !batch_commands.is_empty() {
            let merged = self.store.merge_all_commands(batch_commands)?;
            if !merged.is_empty() {
                total_effects += merged.effects.len();
                let r = self.submit_command_inner(env, merged).await?;
                effect_report.attempted += r.effect_report.attempted;
                effect_report.dispatched += r.effect_report.dispatched;
                effect_report.failed += r.effect_report.failed;
            }
        }

        // Re-run deferred hooks serially, each against a fresh snapshot
        for hook_idx in deferred_indices {
            let snap = self.store.snapshot();
            let ctx = base_ctx.clone().with_snapshot(snap.clone());
            let mut cmd = hooks[hook_idx].hook.run(&ctx).await?;
            if cmd.base_revision().is_none() {
                cmd = cmd.with_base_revision(snap.revision());
            }
            if !cmd.is_empty() {
                total_effects += cmd.effects.len();
                let r = self.submit_command_inner(env, cmd).await?;
                effect_report.attempted += r.effect_report.attempted;
                effect_report.dispatched += r.effect_report.dispatched;
                effect_report.failed += r.effect_report.failed;
            }
        }

        Ok((total_effects, effect_report))
    }

    fn filter_hooks<'a>(env: &'a ExecutionEnv, ctx: &PhaseContext) -> Vec<&'a TaggedPhaseHook> {
        let hooks = env.hooks_for_phase(ctx.phase);
        let active_hook_filter = &ctx.agent_spec.active_hook_filter;
        hooks
            .iter()
            .filter(|tagged| {
                active_hook_filter.is_empty() || active_hook_filter.contains(&tagged.plugin_id)
            })
            .collect()
    }

    /// Run hooks in parallel, returning (hook_index, command) pairs for non-empty results.
    async fn run_hooks_indexed(
        hooks: &[&TaggedPhaseHook],
        base_ctx: &PhaseContext,
        snapshot: &Snapshot,
    ) -> Result<Vec<(usize, StateCommand)>, StateError> {
        let results = join_all(hooks.iter().enumerate().map(|(i, tagged)| {
            let hook = tagged.hook.clone();
            let hook_snapshot = snapshot.clone();
            let hook_ctx = base_ctx.clone().with_snapshot(hook_snapshot.clone());
            async move {
                let mut cmd = hook.run(&hook_ctx).await?;
                if cmd.base_revision().is_none() {
                    cmd = cmd.with_base_revision(hook_snapshot.revision());
                }
                Ok::<(usize, StateCommand), StateError>((i, cmd))
            }
        }))
        .await;

        let mut indexed = Vec::new();
        for result in results {
            let (i, cmd) = result?;
            if !cmd.is_empty() {
                indexed.push((i, cmd));
            }
        }
        Ok(indexed)
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

/// Check whether any Exclusive key appears in more than one command.
fn has_exclusive_key_overlap(
    commands: &[(usize, StateCommand)],
    strategy: impl Fn(&str) -> MergeStrategy,
) -> bool {
    let mut seen: HashSet<&str> = HashSet::new();
    for (_, cmd) in commands {
        for key in &cmd.patch.touched_keys {
            if strategy(key) == MergeStrategy::Exclusive && !seen.insert(key.as_str()) {
                return true;
            }
        }
    }
    false
}

/// Greedily partition commands into a compatible batch and deferred hook indices.
///
/// Walks commands in registration order. A command is added to the batch if none
/// of its Exclusive keys overlap with keys already in the batch; otherwise its
/// hook index is deferred for serial re-execution.
fn partition_commands(
    commands: Vec<(usize, StateCommand)>,
    strategy: impl Fn(&str) -> MergeStrategy,
) -> (Vec<StateCommand>, Vec<usize>) {
    let mut batch_exclusive_keys: HashSet<String> = HashSet::new();
    let mut batch = Vec::new();
    let mut deferred = Vec::new();

    for (hook_idx, cmd) in commands {
        let conflicts = cmd.patch.touched_keys.iter().any(|k| {
            strategy(k) == MergeStrategy::Exclusive && batch_exclusive_keys.contains(k.as_str())
        });

        if conflicts {
            deferred.push(hook_idx);
        } else {
            for k in &cmd.patch.touched_keys {
                if strategy(k) == MergeStrategy::Exclusive {
                    batch_exclusive_keys.insert(k.clone());
                }
            }
            batch.push(cmd);
        }
    }

    (batch, deferred)
}
