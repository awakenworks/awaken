//! Own the actual process pipes; protocol framing stays in the caller.

use std::process::ExitCode;

#[cfg(all(unix, feature = "hand"))]
mod pipes {
    use std::io;
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::fs::FileTypeExt;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite};
    use tokio::net::unix::pipe::{Receiver, Sender};

    pub(super) struct Output(Option<Box<dyn AsyncWrite + Send + Unpin>>);
    type Input = Box<dyn AsyncRead + Send + Unpin>;

    fn socket(fd: OwnedFd) -> io::Result<tokio::net::UnixStream> {
        let socket = std::os::unix::net::UnixStream::from(fd);
        socket.set_nonblocking(true)?;
        tokio::net::UnixStream::from_std(socket)
    }

    fn is_socket(fd: &OwnedFd) -> io::Result<bool> {
        Ok(std::fs::File::from(fd.try_clone()?)
            .metadata()?
            .file_type()
            .is_socket())
    }

    pub(super) fn take() -> io::Result<(Input, Output)> {
        // Tokio's global stdin uses an uncancellable blocking read; its global
        // stdout shutdown does not close fd 1. These roles require real pipe EOF
        // while the reverse direction is still live. Safe descriptor duplication
        // gives the async pipe sole ownership of the original stdout endpoint.
        let input = io::stdin().as_fd().try_clone_to_owned()?;
        let output = io::stdout().as_fd().try_clone_to_owned()?;
        // Node/libuv may supply socket pairs for child stdio; container exec
        // supplies pipes. Both are byte channels, with the same EOF contract.
        let input: Input = if is_socket(&input)? {
            Box::new(socket(input)?)
        } else {
            Box::new(Receiver::from_owned_fd(input)?)
        };
        let output: Box<dyn AsyncWrite + Send + Unpin> = if is_socket(&output)? {
            Box::new(socket(output)?)
        } else {
            Box::new(Sender::from_owned_fd(output)?)
        };
        let null = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/null")?;
        rustix::stdio::dup2_stdin(&null)?;
        rustix::stdio::dup2_stdout(&null)?;
        Ok((input, Output(Some(output))))
    }

    impl AsyncWrite for Output {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.0.as_mut() {
                Some(output) => Pin::new(output).poll_write(cx, bytes),
                None => Poll::Ready(Err(io::ErrorKind::BrokenPipe.into())),
            }
        }

        fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.0.as_mut() {
                Some(output) => Pin::new(output).poll_flush(cx),
                None => Poll::Ready(Ok(())),
            }
        }

        fn poll_shutdown(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
            self.0.take();
            Poll::Ready(Ok(()))
        }
    }
}

#[cfg(all(unix, feature = "hand"))]
pub(super) async fn run(args: &[String]) -> ExitCode {
    let [role, flag, path] = args else {
        eprintln!("usage: awaken-sandbox channel-listen|channel-connect --unix PATH");
        return ExitCode::FAILURE;
    };
    if flag != "--unix" || !std::path::Path::new(path).is_absolute() {
        eprintln!("socket channel requires --unix and an absolute path");
        return ExitCode::FAILURE;
    }
    let result = async {
        let (input, output) = pipes::take().map_err(|error| error.to_string())?;
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .map_err(|error| error.to_string())?;
        let serving = async {
            if role == "channel-listen" {
                awaken_connection_plan::socket_stdio::listen(path, input, output).await
            } else {
                awaken_connection_plan::socket_stdio::connect(path, input, output).await
            }
            .map_err(|error| error.to_string())
        };
        // TERM cancels the bridge future so its owned socket entry is removed.
        // No blocking stdin task remains to hold the Tokio runtime open.
        tokio::select! { result = serving => result, _ = terminate.recv() => Ok(()) }
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("awaken-sandbox {role}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(not(all(unix, feature = "hand")))]
pub(super) async fn run(_args: &[String]) -> ExitCode {
    eprintln!("socket channel roles require Unix and --features hand");
    ExitCode::FAILURE
}
