// @formal surface requirement=runtime.builtin_tools.tool_registration
// @formal reason: Private Windows process ownership for the existing Bash tool.
// @formal reason: Native teardown is qualified by real descendant regression tests.
//! A kill-on-close Job Object owns Bash and every descendant from spawn.
use awaken_process_win32::{OwnedProcessTree, ProcessTreeControl};
use std::io;
use std::process::ExitStatus;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::process::{ChildStderr, ChildStdin, ChildStdout, Command};

type OwnedChild = OwnedProcessTree;

#[derive(Clone)]
pub(crate) struct ProcessTree {
    child: Arc<Mutex<Option<OwnedChild>>>,
    control: ProcessTreeControl,
}

impl ProcessTree {
    fn slot(&self) -> MutexGuard<'_, Option<OwnedChild>> {
        // This mutex holds only an owned native process capability. Taking it
        // or requesting termination cannot invalidate application data; even
        // after a panic, cleanup must retain the capability rather than leak it.
        self.child
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub(crate) fn start_kill(&self) -> io::Result<()> {
        self.control.start_kill()
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
    pub(crate) fn spawn(mut command: Command) -> io::Result<Self> {
        let mut child = OwnedProcessTree::spawn(&mut command)?;
        let control = child.process_tree_control();
        let stdin = child.stdin().take();
        let stdout = child.stdout().take();
        let stderr = child.stderr().take();
        Ok(Self {
            stdin,
            stdout,
            stderr,
            tree: ProcessTree {
                child: Arc::new(Mutex::new(Some(child))),
                control,
            },
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
        self.tree.start_kill()?;
        if let Some(mut child) = self.tree.take() {
            child.wait().await?;
        }
        self.tree.control.wait_for_quiescence().await
    }
}

impl Drop for Child {
    fn drop(&mut self) {
        // A cancellation source may outlive its owner. Retaining that clone
        // must not keep the workload alive after the Bash owner is dropped.
        let _ = self.tree.start_kill();
    }
}

#[cfg(test)]
mod tests;
