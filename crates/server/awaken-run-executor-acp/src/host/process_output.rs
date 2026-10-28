//! Bounded host probe/installer output through the same local process owner.
use awaken_local_process::LocalProcess;
use awaken_provisioning_contract::{ProcessHandle, Signal};
use std::{io, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
};

pub(super) struct HostProcessOutput {
    pub exit_code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

async fn read_pipe<P: AsyncRead + Unpin>(pipe: Option<P>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    if let Some(pipe) = pipe {
        pipe.take(65_537).read_to_end(&mut bytes).await?;
        if bytes.len() > 65_536 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "host process output exceeded its bound",
            ));
        }
    }
    Ok(bytes)
}

pub(super) async fn output(
    command: &mut Command,
    limit: Duration,
) -> io::Result<HostProcessOutput> {
    let mut process = LocalProcess::spawn(command)?;
    let stdout = process.take_stdout();
    let stderr = process.take_stderr();
    let result = tokio::time::timeout(limit, async {
        let (stdout, stderr, status) =
            tokio::try_join!(read_pipe(stdout), read_pipe(stderr), async {
                process.wait().await.map_err(io::Error::other)
            })?;
        Ok(HostProcessOutput {
            exit_code: status.code,
            stdout,
            stderr,
        })
    })
    .await
    .unwrap_or_else(|_| {
        Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "host process timed out",
        ))
    });
    if result.is_err() {
        let _ = process.signal(Signal::Kill).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), process.wait()).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Stdio;

    #[test]
    #[ignore = "child helper invoked by the bounded probe test"]
    fn probe_child() {
        match std::env::var("AWAKEN_PROBE_TEST_MODE").as_deref() {
            Ok("overflow") => {
                use std::io::Write;
                std::io::stdout().write_all(&vec![b'x'; 80_000]).unwrap();
                std::io::stdout().flush().unwrap();
            }
            Ok("timeout") => std::thread::sleep(Duration::from_secs(30)),
            _ => panic!("helper requires an explicit mode"),
        }
    }

    #[tokio::test]
    async fn probe_output_overflow_and_timeout_are_bounded_failures() {
        for (mode, limit, expected) in [
            (
                "overflow",
                Duration::from_secs(5),
                io::ErrorKind::InvalidData,
            ),
            (
                "timeout",
                Duration::from_millis(100),
                io::ErrorKind::TimedOut,
            ),
        ] {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--ignored",
                    "--exact",
                    "host::process_output::tests::probe_child",
                    "--nocapture",
                ])
                .env("AWAKEN_PROBE_TEST_MODE", mode)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let error = match output(&mut command, limit).await {
                Ok(_) => panic!("{mode} must fail closed"),
                Err(error) => error,
            };
            assert_eq!(error.kind(), expected, "{mode}");
        }
    }
}
