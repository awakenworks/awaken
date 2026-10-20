// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Private Windows process ownership for the existing Bash tool.
// @formal reason: Native teardown is qualified by real descendant regression tests.
//! A kill-on-close Job Object owns Bash and every descendant from spawn.
use process_wrap::tokio::{ChildWrapper, CommandWrap, JobObject, KillOnDrop};
use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

type OwnedChild = Box<dyn ChildWrapper>;

#[derive(Clone)]
pub(crate) struct ProcessTree(Arc<Mutex<Option<OwnedChild>>>);

impl ProcessTree {
    fn slot(&self) -> MutexGuard<'_, Option<OwnedChild>> {
        // This mutex holds only an owned native process capability. Taking it
        // or requesting termination cannot invalidate application data; even
        // after a panic, cleanup must retain the capability rather than leak it.
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn start_kill(&self) -> io::Result<()> {
        match self.slot().as_mut() {
            Some(child) => child.start_kill(),
            None => Ok(()),
        }
    }

    fn take(&self) -> Option<OwnedChild> {
        self.slot().take()
    }
}

pub(crate) struct Child {
    pub(crate) stdin: Option<ChildStdin>,
    pub(crate) stdout: Option<ChildStdout>,
    pub(crate) stderr: Option<ChildStderr>,
    tree: ProcessTree,
}

impl Child {
    pub(crate) fn spawn(command: Command) -> io::Result<Self> {
        let mut command = CommandWrap::from(command);
        // JobObject suspends before user code, assigns the whole lifetime,
        // then resumes. No descendant can race an after-spawn PID attachment.
        command.wrap(KillOnDrop).wrap(JobObject);
        let mut child = command.spawn()?;
        let stdin = child.stdin().take();
        let stdout = child.stdout().take();
        let stderr = child.stderr().take();
        Ok(Self {
            stdin,
            stdout,
            stderr,
            tree: ProcessTree(Arc::new(Mutex::new(Some(child)))),
        })
    }

    pub(crate) fn process_tree(&self) -> ProcessTree {
        self.tree.clone()
    }

    pub(crate) async fn wait(&mut self) -> io::Result<ExitStatus> {
        let mut child = self
            .tree
            .take()
            .ok_or_else(|| io::Error::other("Bash process owner already consumed"))?;
        // The local box retains kill-on-drop when this wait is cancelled.
        child.wait().await
    }

    pub(crate) async fn shutdown(&mut self) -> io::Result<()> {
        if let Some(mut child) = self.tree.take() {
            child.start_kill()?;
            child.wait().await?;
        }
        Ok(())
    }
}
