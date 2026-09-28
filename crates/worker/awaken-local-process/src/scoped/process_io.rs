// @formal evidence sandbox.scoped_launch_requires_current_grant_and_init_exit_for_quiescence
//! Business stdio is descriptor-backed, never mixed with bootstrap framing.
use awaken_provisioning_contract::Stdio;
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
};
use tokio::net::unix::pipe::{self, Receiver, Sender};

/// Business stream selection is explicit; AgentChannel overrides stdout/stdin
/// while preserving the provider's stderr policy without a forwarding task.
#[derive(Clone, Copy)]
pub struct ScopedStdio {
    mode: Stdio,
    agent_stderr: Option<bool>,
}

impl From<Stdio> for ScopedStdio {
    fn from(mode: Stdio) -> Self {
        Self {
            mode,
            agent_stderr: None,
        }
    }
}

impl ScopedStdio {
    pub fn agent(inherit_stderr: bool) -> Self {
        Self {
            mode: Stdio::Piped,
            agent_stderr: Some(inherit_stderr),
        }
    }
}

#[derive(Default)]
pub(super) struct ProcessIo {
    pub stdin: Option<Sender>,
    pub stdout: Option<Receiver>,
    pub stderr: Option<Receiver>,
}

impl ProcessIo {
    pub fn new(selection: ScopedStdio) -> io::Result<(Self, Vec<OwnedFd>)> {
        let (mut io, mut handles) = Self::for_mode(selection.mode)?;
        if let Some(inherit) = selection.agent_stderr {
            handles[2] = if inherit {
                std::io::stderr().as_fd().try_clone_to_owned()?
            } else {
                std::fs::OpenOptions::new()
                    .write(true)
                    .open("/dev/null")?
                    .into()
            };
            io.stderr = None;
        }
        Ok((io, handles))
    }

    fn for_mode(mode: Stdio) -> io::Result<(Self, Vec<OwnedFd>)> {
        match mode {
            Stdio::Inherit => Ok((
                Self::default(),
                vec![
                    std::io::stdin().as_fd().try_clone_to_owned()?,
                    std::io::stdout().as_fd().try_clone_to_owned()?,
                    std::io::stderr().as_fd().try_clone_to_owned()?,
                ],
            )),
            Stdio::Null => {
                let file = std::fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("/dev/null")?;
                Ok((
                    Self::default(),
                    (0..3)
                        .map(|_| file.as_fd().try_clone_to_owned())
                        .collect::<io::Result<_>>()?,
                ))
            }
            Stdio::Piped => {
                let (stdin, input) = pipe::pipe()?;
                let (output, stdout) = pipe::pipe()?;
                let (error, stderr) = pipe::pipe()?;
                Ok((
                    Self {
                        stdin: Some(stdin),
                        stdout: Some(stdout),
                        stderr: Some(stderr),
                    },
                    vec![
                        input.into_blocking_fd()?,
                        output.into_blocking_fd()?,
                        error.into_blocking_fd()?,
                    ],
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn agent_stdio_preserves_exact_stderr_without_a_forwarder() {
        // IO4 AgentChannel requires independent stdin/stdout pipes in both
        // policies. stderr inherits the exact caller descriptor OR /dev/null;
        // it never leaves an unread business pipe that could deadlock or SIGPIPE.
        for inherit in [false, true] {
            let (io, handles) = ProcessIo::new(ScopedStdio::agent(inherit)).unwrap();
            assert!(io.stdin.is_some() && io.stdout.is_some() && io.stderr.is_none());
            let expected: OwnedFd = if inherit {
                std::io::stderr().as_fd().try_clone_to_owned().unwrap()
            } else {
                std::fs::File::open("/dev/null").unwrap().into()
            };
            let expected = rustix::fs::fstat(&expected).unwrap();
            let actual = rustix::fs::fstat(&handles[2]).unwrap();
            assert_eq!(
                (actual.st_dev, actual.st_ino),
                (expected.st_dev, expected.st_ino)
            );
            assert_eq!(handles.len(), 3);
        }
    }

    #[tokio::test]
    async fn each_stdio_mode_has_exact_ownership_and_kernel_endpoints() {
        // IO1 Inherit => clones the actual three stdio handles, no parent pipes;
        // IO2 Null => three /dev/null endpoints, no parent pipes;
        // IO3 Piped => three parent pipes and distinct child pipe endpoints.
        // All modes return exactly three independently owned CLOEXEC descriptors.
        // Real scoped binary tests cover end-to-end bytes and socket exclusion.
        for mode in [Stdio::Inherit, Stdio::Null, Stdio::Piped] {
            let (io, handles) = ProcessIo::new(mode.into()).unwrap();
            assert_eq!(handles.len(), 3);
            let piped = matches!(mode, Stdio::Piped);
            assert_eq!(io.stdin.is_some(), piped);
            assert_eq!(io.stdout.is_some(), piped);
            assert_eq!(io.stderr.is_some(), piped);
            let inherited = [
                std::io::stdin().as_fd().try_clone_to_owned().unwrap(),
                std::io::stdout().as_fd().try_clone_to_owned().unwrap(),
                std::io::stderr().as_fd().try_clone_to_owned().unwrap(),
            ];
            let null = std::fs::File::open("/dev/null").unwrap();
            let mut pipe_identities = std::collections::BTreeSet::new();
            for (index, handle) in handles.iter().enumerate() {
                assert!(
                    rustix::io::fcntl_getfd(handle)
                        .unwrap()
                        .contains(rustix::io::FdFlags::CLOEXEC)
                );
                let actual = rustix::fs::fstat(handle).unwrap();
                let expected = match mode {
                    Stdio::Inherit => rustix::fs::fstat(&inherited[index]).unwrap(),
                    Stdio::Null => rustix::fs::fstat(&null).unwrap(),
                    Stdio::Piped => {
                        assert!(pipe_identities.insert((actual.st_dev, actual.st_ino)));
                        assert_eq!(
                            rustix::fs::FileType::from_raw_mode(actual.st_mode),
                            rustix::fs::FileType::Fifo
                        );
                        continue;
                    }
                };
                assert_eq!(
                    (actual.st_dev, actual.st_ino),
                    (expected.st_dev, expected.st_ino)
                );
            }
        }
    }
}
