//! Every process one Session environment starts, and what each one owes.
//!
//! # Why this is one module
//!
//! A container environment has two spawn edges — an opaque stdio agent and a
//! plain process — and for a long time only one of them took a responsibility
//! record. The other started real processes that nothing counted, so disposal
//! read "nothing was started" and skipped its sweep outright for any
//! environment that only ever used it. Keeping both edges here, beside the
//! gate that closes them and the sweep that reads what they recorded, is what
//! stops the two from drifting apart again: the record, not the kind of
//! process, is what cleanup acts on.

use awaken_provisioning_contract as pc;

use crate::exec_attempts::DisposalSeal;
use crate::{
    ContainerRuntime, ContainerSandbox, ContainerState, RuntimeAgentProcess, RuntimeError,
};

impl<R: ContainerRuntime + 'static> ContainerSandbox<R> {
    /// Start an opaque stdio agent as an exec process in this Session environment.
    /// Repeated calls create independent attempt processes while preserving the same
    /// workspace, mounts, network policy, and durable sandbox handle.
    pub async fn spawn_agent(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError> {
        let command = self.materialize_command(command).await?;
        self.claimed_spawn_agent(command)?.await
    }

    /// Start one agent exec whose responsibility record exists before the exec
    /// can begin.
    ///
    /// [`pc::ClaimedEffect::new`] takes the claim by value, so the attempt is
    /// counted and the retention is minted strictly before the future is built
    /// and therefore before any poll reaches the runtime. A task cancelled
    /// between the exec starting and the [`RuntimeAgentProcess`] reaching its
    /// caller drops this value unsettled, which is the one transition that runs
    /// the retention and leaves the attempt on record for
    /// [`ContainerSandbox::prepare_disposal`] to reap.
    ///
    /// The record is in-process on purpose and is the weakest of the three
    /// claim kinds: nothing outside this process can read it. What survives the
    /// process is the enumerable pid file the exec wrapper wrote, which the
    /// same disposal reaps by listing rather than by remembering.
    fn claimed_spawn_agent(
        &self,
        command: pc::MaterializedCommand,
    ) -> Result<pc::ClaimedEffect<'_, RuntimeAgentProcess>, pc::SandboxError> {
        let admitted = self.admit_exec_attempt()?;
        let attempt = admitted.attempt().to_string();
        let execs = self.execs.clone();
        Ok(pc::ClaimedEffect::new(
            pc::EffectClaim::in_process(move || execs.retain_orphan(attempt)),
            async move {
                self.runtime
                    .spawn_agent(admitted, command)
                    .await
                    .map_err(pc::SandboxError::from)
            },
        ))
    }

    /// The same obligation for the plain process edge.
    ///
    /// Both edges start a real process inside the container and both write the
    /// wrapper's pid file, so both owe the same record. This one had none: its
    /// attempts were never counted, so disposal read "nothing was started" and
    /// skipped the sweep outright for any environment that only ever used it.
    pub(crate) fn claimed_spawn(
        &self,
        command: pc::MaterializedCommand,
    ) -> Result<pc::ClaimedEffect<'_, Box<dyn pc::ProcessHandle>>, pc::SandboxError> {
        let admitted = self.admit_exec_attempt()?;
        let attempt = admitted.attempt().to_string();
        let execs = self.execs.clone();
        Ok(pc::ClaimedEffect::new(
            pc::EffectClaim::in_process(move || execs.retain_orphan(attempt)),
            async move {
                self.runtime
                    .spawn(admitted, command)
                    .await
                    .map_err(pc::SandboxError::from)
            },
        ))
    }

    /// Take this environment's next exec attempt, or refuse because disposal
    /// has sealed it.
    ///
    /// Every spawn edge passes through here, because what disposal reads is
    /// the record, not the kind of process. A sealed environment refuses:
    /// its reaper enumerates the container once, so a process admitted after
    /// that sweep would still be running — and still writing — when Secret
    /// write-back and the Memory harvest read the workspace.
    fn admit_exec_attempt(&self) -> Result<crate::AdmittedExec, pc::SandboxError> {
        self.execs.admit(&self.container_id).ok_or_else(|| {
            pc::SandboxError::Gone(format!(
                "Sandbox `{}` is being disposed and cannot start another exec",
                self.id
            ))
        })
    }

    /// Start one exec that belongs to this environment's own machinery rather
    /// than to Session work.
    ///
    /// Cleanup seals the Session edges and then reads the workspace through
    /// execs of its own. Those are the sweep, not something that could outlive
    /// it, so they pass the gate — and they still take a record, because the
    /// container started them like any other.
    pub(crate) async fn spawn_machinery_agent(
        &self,
        command: pc::Command,
    ) -> Result<RuntimeAgentProcess, pc::SandboxError> {
        let command = self.materialize_command(command).await?;
        let admitted = self.execs.admit_machinery(&self.container_id);
        let attempt = admitted.attempt().to_string();
        let execs = self.execs.clone();
        pc::ClaimedEffect::new(
            pc::EffectClaim::in_process(move || execs.retain_orphan(attempt)),
            async move {
                self.runtime
                    .spawn_agent(admitted, command)
                    .await
                    .map_err(pc::SandboxError::from)
            },
        )
        .await
    }

    /// Exec attempts started here whose caller never received the process.
    /// Read by disposal and by the cancellation tests; an empty list is not
    /// proof that no exec is running, only that none was dropped unsettled.
    #[cfg(test)]
    pub(crate) fn orphaned_exec_attempts(&self) -> Vec<String> {
        self.execs.orphaned()
    }

    /// Close this environment to new exec attempts — the exact call disposal
    /// makes, so a test can observe the gate without racing a real cleanup.
    #[cfg(test)]
    pub(crate) fn seal_exec_attempts(&self) -> DisposalSeal {
        self.execs.seal()
    }

    #[cfg(test)]
    pub(crate) fn exec_attempts_sealed(&self) -> bool {
        self.execs.sealed()
    }

    /// Make the container stop every exec attempt this environment started.
    ///
    /// Gated on "an attempt was started" rather than on "an orphan is on
    /// record": the record only sees spawns dropped inside this process, while
    /// the container's pid files see every attempt, including ones whose
    /// `ProcessHandle` was simply dropped without a wait. The gate is also what
    /// keeps the demand honest — a Session environment that never exec'd cannot
    /// be holding an orphan, so a runtime with no exec support is never asked
    /// to reap and its fail-closed inherited implementation stays unreachable.
    pub(crate) async fn reap_exec_orphans(&self) -> Result<DisposalSeal, pc::SandboxError> {
        // Closing and asking are one step. Closing must happen even when there
        // is nothing to reap, or an environment that never exec'd could
        // acquire its first process during its own cleanup. The seal is
        // returned rather than dropped because the workspace reads that follow
        // are exactly what it makes sound.
        let seal = self.execs.seal();
        if !seal.any_started() {
            return Ok(seal);
        }
        if let Err(error) = self.runtime.reap_exec_orphans(&self.container_id).await {
            // A container that is no longer running has already stopped every
            // exec it started, which is the whole of what reaping is for, so
            // refusing to dispose it would strand the environment over an
            // obligation already met. The tolerance is closed on purpose: only
            // an observation proving the container is not running admits it,
            // because a live container that refuses to reap may still be
            // writing while the harvest below reads.
            let already_stopped = match self.runtime.inspect(&self.container_id).await {
                Ok(ContainerState::Running) => false,
                Ok(_) | Err(RuntimeError::NotFound(_)) => true,
                Err(_) => false,
            };
            if !already_stopped {
                return Err(error.into());
            }
        }
        self.execs.clear_orphans();
        Ok(seal)
    }
}
