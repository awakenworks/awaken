//! Business stdio is descriptor-backed, never mixed with bootstrap framing.
use awaken_provisioning_contract::Stdio;
use std::{
    io,
    os::fd::{AsFd, OwnedFd},
};
use tokio::net::unix::pipe::{self, Receiver, Sender};

#[derive(Default)]
pub(super) struct ProcessIo {
    pub stdin: Option<Sender>,
    pub stdout: Option<Receiver>,
    pub stderr: Option<Receiver>,
}

impl ProcessIo {
    pub fn new(mode: Stdio) -> io::Result<(Self, Vec<OwnedFd>)> {
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
    async fn each_stdio_mode_has_exact_ownership_and_kernel_endpoints() {
        // IO1 Inherit => clones the actual three stdio handles, no parent pipes;
        // IO2 Null => three /dev/null endpoints, no parent pipes;
        // IO3 Piped => three parent pipes and distinct child pipe endpoints.
        // All modes return exactly three independently owned CLOEXEC descriptors.
        // Real scoped binary tests cover end-to-end bytes and socket exclusion.
        for mode in [Stdio::Inherit, Stdio::Null, Stdio::Piped] {
            let (io, handles) = ProcessIo::new(mode).unwrap();
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
