// @formal surface obligation=sandbox.local_process_spawn_policy_and_waiter_lock_ownership_are_preserved
// @formal reason: Original Windows Job assignment, independent control and retained member exit observation.
//! Native process-tree ownership using the publishable registry dependency.
use process_wrap::tokio::{
    ChildWrapper, CommandWrap, CommandWrapper, JobObject, JobObjectChild, KillOnDrop,
};
use std::{any::Any, io, process::ExitStatus, time::Duration};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

/// Control for the one original supervision Job, independent of child waiting.
#[derive(Clone, Debug)]
pub struct ProcessTreeControl(super::sys::Job);

impl ProcessTreeControl {
    /// Request owned-tree termination. A request alone is not an exit receipt.
    pub fn start_kill(&self) -> io::Result<()> {
        self.0.terminate()
    }
    /// Observe retained member exits and fresh original-Job accounting after
    /// termination was requested. Returns false before that first request.
    /// Query errors or concurrent membership changes preserve uncertainty.
    pub fn is_quiescent(&self) -> io::Result<bool> {
        self.0.is_quiescent()
    }
    /// Join a prior termination request on this same original supervision Job.
    /// Call `start_kill` first, or let the owned leader's wait initiate it.
    /// Cancellation retains every original member observation for a retry.
    pub async fn wait_for_quiescence(&self) -> io::Result<()> {
        while !self.is_quiescent()? {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    }
}

/// Installed privately before JobObject. All dependency pre_spawn hooks run
/// before any wrap_child, so its leader is still suspended at this assignment.
/// JobObject then establishes its nested private Job and resumes that leader.
/// This wrapper returns the exact same child and never resumes or unwraps it.
#[derive(Debug)]
struct AssignOriginalJob(super::sys::Job);

impl CommandWrapper for AssignOriginalJob {
    fn wrap_child(
        &mut self,
        mut child: Box<dyn ChildWrapper>,
        _: &CommandWrap,
    ) -> io::Result<Box<dyn ChildWrapper>> {
        let assigned = match child.process_handle() {
            Some(handle) => self.0.assign(handle),
            None => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "native child did not retain its original process handle",
            )),
        };
        if let Err(error) = assigned {
            let _ = child.start_kill();
            return Err(error);
        }
        Ok(child)
    }
}

/// A private leader and original supervision Job assigned before workload code.
/// Only pipes and a cloned same-Job control escape; never a raw native child.
pub struct OwnedProcessTree {
    child: Box<dyn ChildWrapper>,
    control: ProcessTreeControl,
}

impl OwnedProcessTree {
    /// Create the supervision Job before spawn. Assignment failure never falls
    /// back to an unowned child; the configured Command is restored for callers.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let control = ProcessTreeControl(super::sys::Job::new()?);
        let original = std::mem::replace(command, Command::new(""));
        let mut wrapped = CommandWrap::from(original);
        // Fixed private ordering is part of this component's launch invariant.
        wrapped
            .wrap(AssignOriginalJob(control.0.clone()))
            .wrap(JobObject)
            .wrap(KillOnDrop);
        let result = wrapped.spawn();
        *command = wrapped.into_command();
        let mut child = result?;
        if !(child.as_ref() as &dyn Any).is::<JobObjectChild>() {
            let _ = control.start_kill();
            let _ = child.start_kill();
            return Err(io::Error::other(
                "original suspended Job launch boundary was not retained",
            ));
        }
        Ok(Self { child, control })
    }
    /// Clone control for the same original Job, including during active wait.
    pub fn process_tree_control(&self) -> ProcessTreeControl {
        self.control.clone()
    }
    /// Original leader identity for diagnostics; never an ownership lookup key.
    pub fn id(&self) -> Option<u32> {
        self.child.id()
    }
    /// Detach configured input without exposing a child ownership escape.
    pub fn stdin(&mut self) -> &mut Option<ChildStdin> {
        self.child.stdin()
    }
    /// Detach configured output without exposing a child ownership escape.
    pub fn stdout(&mut self) -> &mut Option<ChildStdout> {
        self.child.stdout()
    }
    /// Detach configured error output without exposing a child ownership escape.
    pub fn stderr(&mut self) -> &mut Option<ChildStderr> {
        self.child.stderr()
    }
    /// Request original-Job termination without claiming physical completion.
    pub fn start_kill(&mut self) -> io::Result<()> {
        self.control.start_kill()
    }

    /// After leader exit, request termination and expose status only after
    /// retained member handles report exit and fresh Job accounting is zero.
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        // The fixed launch sequence leaves JobObject over exactly the native
        // child. Observe that direct lower child without consuming dependency
        // completion-port messages or relying on its cached wait status.
        let Some(status) = self.child.inner_mut().try_wait()? else {
            return Ok(None);
        };
        self.control.start_kill()?;
        Ok(self.control.is_quiescent()?.then_some(status))
    }
    /// Cancellation consumes neither original Job identity nor exit proof.
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        loop {
            if let Some(status) = self.try_wait()? {
                return Ok(status);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
}

impl Drop for OwnedProcessTree {
    fn drop(&mut self) {
        // Independent accepted controls keep the Job handle alive. Owner drop
        // must request termination explicitly rather than defer to last close.
        let _ = self.control.start_kill();
    }
}
